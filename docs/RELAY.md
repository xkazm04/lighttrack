# RELAY — cloud→device task queue for local Claude Code execution

Deployed apps enqueue heavy, offline-tolerant LLM tasks on the cloud LightTrack instance; an enrolled
local device (running the user's Claude Code subscription) leases the ones it advertises the
capability for, over outbound HTTPS, executes them against a local action library, pushes results
back into the apps, and logs every run to LightTrack. The point: route heavy LLM work that doesn't
need an online reaction through the flat-rate Claude subscription instead of metered APIs, while the
Gemini production engine keeps serving the latency-sensitive paths.

## Why pull, not push

The local device sits behind NAT with no stable public IP. Instead of tunnels or inbound ports,
the device **long-polls the cloud** with a device key — outbound-only, nothing to expose.
"One specific device" is an authorization fact (only the enrolled key can lease), not a
networking fact.

The second security property falls out of the payload contract: the cloud stores and forwards
only `action_type` + JSON params. Prompts, allowed tools, and connector credentials live only on
the device, in a gitignored library. A compromised cloud or app key can invoke predefined actions
with parameters — never make the device run arbitrary Claude Code.

```
xprice app A ──┐  POST /v1/relay/tasks (project API key, idempotency key)
xprice app B ──┼────────────► LightTrack Cloud
LightTrack ────┘                 relay_tasks: queued → leased → succeeded | dead
  internal                              ▲
                                        │ outbound lease / result (per-device ltd_… key)
                              lt-agent (local device, one of N enrolled)
                                 │ actions/<type>/  ← gitignored: prompt.md + action.toml + connector
                                 │ claude.exe -p … --output-format json  (engine::invocation::run)
                                 │ connector (http | command) → pushes result into the app
                                 └─ POST /v1/relay/tasks/:id/result (+ usage/cost → cloud logs a priced event)
```

## Task lifecycle

Statuses: `queued → leased → succeeded | dead`, plus `cancelling → cancelled` when an operator stops
a task. A failed attempt goes back to `queued` with the error recorded and `next_attempt_at` pushed
out. Dead-lettering **alerts** through the existing channels (`LIGHTTRACK_ALERT_WEBHOOK` /
`LIGHTTRACK_ALERT_NTFY`, event `relay_task_dead` — see `docs/ALERTS.md`) from every path: a failure
report that exhausts the retries, the pre-lease sweep, and — since M7 — the **timed** sweep in the
API's `schedule_sweep`. That last one closed a real hole: the reap used to run only inside
`lease_relay_tasks`, so a fleet with no device polling (which is exactly what "the device is gone"
looks like from the cloud) never dead-lettered anything and never raised the alert that says so.

- **Retry policy** (per task, defaults): `max_attempts = 4`, `retry_interval_secs = 18000` (5h —
  one Claude subscription usage window). A fully offline device therefore has a ~20h envelope
  before tasks dead-letter.
- **Two budgets, kept apart.** `failures` counts runs that actually ran and reported a failure — it
  is the retry budget, measured against `max_attempts`. `stale_reclaims` counts *device deaths*: a
  lease that expired without a report, reclaimed for another device, capped at
  `RELAY_MAX_STALE_RECLAIMS` (3). One counter could not tell the two apart, and the difference is
  load-bearing: a task whose device dies every time never reports a failure, so a `max_attempts`-only
  rule either re-leased it forever or dead-lettered work that had never actually been tried.
  `attempts` still counts leases, for observability, and no longer decides anything.
- **`deferred` hands the claim back.** When the device can't attempt at all (subscription window
  exhausted, weekly cap), it settles `deferred` with an optional `retry_after_secs`. It records no
  failure: a closed window is not the action failing.
- **Duplicate result reports are refused, not re-applied.** A settle on a terminal task answers
  `409` with what the record actually holds. Delivery is at-least-once end to end, so connectors
  must be idempotent — the `idempotency_key` is carried on the task for exactly that purpose, and
  re-enqueueing with the same key returns the existing task instead of a duplicate.

## The lease is fenced and renewable (M7)

`lease_secs` used to be two quantities wearing one name: how long a Claude Code run may legitimately
take, **and** how long a vanished device may go unnoticed. That is why it was clamped to six hours —
and why a dead device's task sat untouchable for most of a day. Those are now separate:

- a lease is granted for minutes (60s–1800s), and the holder **renews it on a timer** at the
  `renew_secs` cadence the lease response names (a third of the TTL, so two consecutive misses — a
  sleeping laptop, a transient error — do not forfeit a live run);
- the staleness window is therefore **detection latency alone**, roughly `3 × renew_secs`, while a
  run takes as long as it takes.

Every lease stamps a **`lease_fence`**: the instant it was granted, carried by every write the holder
makes and compared for exact equality. The check it replaces was `status == "leased"`, which asks
about *liveness* where *ownership* was meant — a task whose lease expired and was re-leased to a
second device is still `leased`, so the first device's late report landed on the second device's run
and overwrote it, silently, with a plausible-looking result. A refused write answers `409` and the
run stops **without delivering**: unlike a stale write to a database row, the delivery half of an
action is a connector call the cloud cannot take back.

Renewal moves the *deadline*, never the fence, so one device's lease keeps one identity for its whole
run. Progress rides its own endpoint and never the heartbeat — the moment liveness is conditioned on
having something to report, a live-but-stuck device reads as a dead one, and those are the two states
the mechanism exists to tell apart.

**Cancellation.** `POST /v1/relay/tasks/:id/cancel` (the task's own project key, or admin): a queued
task becomes `cancelled` outright; a leased one becomes `cancelling`, which is outside the leasable
set — so the reclaim path can never hand a cancelled task to a second device — and its device learns
at its next renewal. Whatever it then reports, the task ends `cancelled` and consumes no retry: an
operator stopped it, so its outcome is not a verdict on the action. Cancelling something already
terminal is a `409`, not a comfortable lie.

## API surface (Phase 1 — shipped)

| Route | Auth | Purpose |
|---|---|---|
| `POST /v1/relay/tasks` | project key | Enqueue `action_type` + `payload` (+ `idempotency_key`, `max_attempts`, `retry_interval_secs`, `source`). A project key is forced into its own project. Answers the task plus an **admission verdict** (`queued { eligible_devices }`), or `422 relay_unroutable` when devices are enrolled and none advertises that action type. |
| `GET /v1/relay/tasks/:id` | project key (own) / admin | Status + result — the originating app's polling fallback. |
| `GET /v1/relay/tasks?project=&status=&limit=` | project key (own) / admin | List/inspect. |
| `POST /v1/relay/lease` | device key | Lease up to `max` due tasks **this device can run**, holding each for `lease_secs` (60s–1800s). Carries `capabilities` + `agent_version`; identity comes from the key, and a client-asserted `device` is ignored. Optional `wait_secs` (≤25) long-polls until a task is due. Answers `{ tasks, lease_secs, renew_secs }` — the granted TTL after clamping, and how often to renew. |
| `POST /v1/relay/tasks/:id/renew` | device key | Heartbeat, carrying `fence`. `409` = the lease is no longer yours: stop, and do not deliver. |
| `POST /v1/relay/tasks/:id/progress` | device key | Liveness detail (`fence` + `progress`), visible on the task. Deliberately not on the heartbeat. |
| `POST /v1/relay/tasks/:id/cancel` | project key (own) / admin | Stop a queued or leased task. `409` if it already finished. |
| `POST /v1/relay/tasks/:id/result` | device key | Settle: `succeeded` (+`result`) \| `failed` (+`error`) \| `deferred` (+`retry_after_secs`), carrying `fence`. Optional usage/accounting: `model`, `input_tokens`, `output_tokens`, `latency_ms`, `cost_usd`, `mode`. Optional provenance (see **Quality model**): `prompt_sha256`, `action_version`, and — only when the action set `report_io` — `input` / `output`. `409` = not held; the result was NOT recorded. |
| `POST /v1/relay/devices` | admin | Enrol a device: `name`, optional `project_id`, `capabilities`. Returns the row **plus its key, once**. Never exposed over MCP. |
| `GET /v1/relay/devices?project=` | admin | The fleet: advertised capabilities, `last_seen_at` / `seen_secs_ago` / `online`, agent version, revocation. Never returns a key or a digest. |
| `DELETE /v1/relay/devices/:id` | admin | Revoke a device. A flag, not a delete — tasks it already ran keep naming a device that still resolves. |

## Enrolment, capabilities, and admission (M18)

Enrolment used to be one shared `LIGHTTRACK_RELAY_DEVICE_KEY`. That is a workable answer for exactly
one device and a bad one for two: the secret cannot be rotated for a single machine, a leak means
re-keying the whole fleet at once, and the `device` written onto a task was whatever the client
asserted — so the cloud's record of *who ran what* was decoration. Multi-device enrolment is no
longer "future work".

**A device is a row.** `POST /v1/relay/devices` mints `ltd_<prefix>_<secret>`, stored as the same
salted digest an API key is and **shown exactly once**. `lt relay devices add --name studio-laptop
--capability 'xprice/*'` is the operator path; `lt relay devices list | revoke <id>` are the others.
Revocation is a flag, not a delete, so tasks that already named a device keep resolving.

**A device advertises what it can run.** `capabilities` are exact action types or `<ns>/*` namespace
prefixes; an **empty** list means "everything". `lt-agent` derives its own inventory from the action
library — every `<ns>/<name>/` holding a `prompt.md` — re-enumerated each poll round and sent on
every lease, so adding an action folder needs no restart and no config edit. A hand-kept capability
list would go stale the moment somebody added a folder, and a stale list *is* the routing failure
this exists to end. A namespace prefix stops at a `/`: `xprice/*` does not cover `xpricey/thing`.

**The lease is filtered, not post-filtered.** The narrowing happens inside the claim, beside the
due/expired predicates, so a task this device cannot run is left `queued` with no fence and no
attempt spent. Applied afterwards, the lease would still have stamped its fence on work it then
dropped — a device silently consuming claims on things it cannot do. Before this, a device whose
library lacked the action leased it anyway, burned a real attempt reporting "no action", and waited
out a five-hour retry interval to do it again.

**Enqueue answers with a verdict.** Validation used to be "`action_type` is non-empty", so a typo was
indistinguishable from a healthy backlog until the task dead-lettered ~20h later having burned every
attempt. Now `POST /v1/relay/tasks` returns `admission: { verdict: "queued", eligible_devices: N }`,
or refuses with **`422 relay_unroutable`** naming the action and the fix. The SDKs surface it:
`RelayError.is_unroutable` / `.isUnroutable` (Python / TS) tells a permanent refusal from a timeout
worth retrying.

`eligible_devices: 0` is **not** a refusal — it means no devices are enrolled at all, which is the
legacy shared-key deployment, and refusing its traffic would be this feature breaking the relay it
hardens. Only an enrolled fleet that advertises nothing matching can refuse.

**`relay_task_unroutable`** closes the half admission cannot see: a task that *was* routable and is
not any more, because the only device with the action was revoked, narrowed on an upgrade, or never
re-enrolled. The M7 schedule sweep re-asks the fleet, and a queued task past
`LIGHTTRACK_RELAY_UNROUTABLE_SECS` (default 900, `0` = off) with zero eligible devices alerts through
the existing channels. Fifteen minutes, not seconds: a fleet is allowed to be briefly empty during a
restart, and it is well inside the five-hour retry interval.

**The legacy key still works, deprecated.** `LIGHTTRACK_RELAY_DEVICE_KEY` authenticates with every
capability and leases unfiltered, exactly as before, and logs a deprecation line at startup naming
what it costs (no routing, no revocation, no liveness, and no way to tell two holders apart). Kept
for one release. The admin key (and dev mode) also passes the device guard, for local testing.

**Never over MCP.** `POST /v1/relay/devices` mints a secret, and a key in a tool result is a key in a
transcript — so device enrolment is HTTP/CLI only however `LIGHTTRACK_MCP_ALLOW_WRITES` is set. What
MCP does get is three read-only tools (`readOnlyHint`): `list_relay_tasks`, `get_relay_task`, and
`list_relay_devices`, the last of which carries no key and no digest.

Store: `relay_tasks` on the `Store` trait, declared as `Surface::Relay` in the capability manifest
(`docs/PARITY.md`); the fleet is its own `Surface::Devices` — a backend can host the task queue and
have no `devices` table, and "nobody is enrolled" is a load-bearing answer there, so it must never be
something a missing table says by accident. SQLite and Postgres serve both; Firestore refuses both,
and the conformance suite asserts every method's refusal. SQLite is the reference implementation; **Postgres implements the full domain**
(`store-pg/src/relay.rs` + `relay_lease.rs` — `FOR UPDATE SKIP LOCKED` leases, transactional fenced
settle), so the Neon-backed cloud serves relay natively. Firestore does not declare the surface, so
every relay method there answers `501 unsupported` and the conformance suite asserts that refusal.

The fence is proved, not assumed: the shared conformance suite's `relay_lease` section drives a
device to lease, renew, report progress, lose its lease to a reclaim, and then settle — and asserts
the late settle is `NotHeld` with the successor's task untouched. There is no way to observe that
race from outside, which is exactly why it is pinned there.

## Cost model: priced from the envelope, then the book, then a flat rate

A relay run is **metered traffic**. A headless `claude -p` bills at API rates (D0), so the flat
$1.00 this used to stamp on every run was not a simplification — it was a wrong number, and every
margin figure computed from relay traffic inherited it. D18 replaces it with three sources, tried in
descending order of how much each is worth trusting:

1. **`envelope`** — `cost_usd` from the device's CLI envelope. The device saw the actual bill; this
   is the price, not evidence beside one. A non-finite or negative figure is refused and falls
   through: a device is not a trusted pricing oracle, and one `NaN` poisons every `SUM` that ever
   reads the row.
2. **`book`** — our own arithmetic: the DB price book (`model_prices`) applied to the tokens the
   device did report. An estimate, but a principled one, and labelled as ours.
3. **`flat`** — `LIGHTTRACK_RELAY_FLAT_COST_USD` (default 1.0). The last resort, for a run that
   reported neither a cost nor priceable tokens. It exists so such a run is still *some* number
   rather than a silent zero.

Which one was used is stamped on the row as `metadata.cost_source`, the **same field the native
ingest door uses** — so a margin query can qualify a relay row exactly as it qualifies any other,
without knowing the relay exists. `metadata.device_cost_usd` still carries what the device reported,
kept even when it equals the billed figure: "we billed the envelope" and "the envelope said X" are
different claims, and only the second survives a later change of pricing policy.

The **cloud logs the event itself on settle** (no project key needed on the device, one writer): a
terminal `succeeded`/`failed` report on a live lease inserts an `LlmEvent` with `provider:
"anthropic"`, the `source`/tokens/latency/`mode` the device reported, `trace_id = task_id` (retries
of one task group into one trace), and `metadata: { task_id, action_type, attempt, cost_source,
device_cost_usd, mode }`. `deferred` logs nothing — no run happened.

## Admission: enqueue is the decision point

The settle-time event is recorded unconditionally, and that is deliberate rather than an oversight:
the run has already happened, and declining to *record* spend does not un-spend it. Refusing there
would only corrupt the cost report.

So the project's limits are checked at **enqueue** instead — the last moment a refusal is still free.
`POST /v1/relay/tasks` runs the same evaluator the status page and the ingest 429 use
(`evaluate_project_limits`), against the same thresholds and with the same `basis` explanation, so a
caller cannot be told two different stories about one cap:

- **Hard breach** (an enforcing rule at its threshold) → **429 `rate_limited`**, with `Retry-After`
  and the breach reason. Nothing is queued.
- **Soft tier** (past `warn_at`, not yet breached) → the task **is** queued, and the response carries
  a `warning` naming the rule. A heads-up, not a refusal.
- **Limits unavailable** (a backend that cannot answer) → admit. An unreachable evaluator is not
  evidence of an exceeded budget, and refusing work on it would turn a degraded read path into an
  outage.
- **Idempotent replay** (a repeated `idempotency_key`) → no budget check. Answering with a task that
  already exists enqueues nothing, and refusing a replay would break idempotency exactly when a
  caller is retrying.

Relay events still show up in costs, usage and forecasts like any other traffic.

## Quality model: a relay run is a judgeable event (M19)

LightTrack is an LLM-as-judge product, and the relay is the one LLM workload it *originates*. It
used to be the only one it could not score: the settle event was written with `input: None,
output: None`, both judges skip an event without content, and the result JSON sat in
`relay_tasks.result` read by nobody. Meanwhile the action's `prompt.md` was edited in place on
disk with no version and no fingerprint, while the prompt registry versions app prompts and gates
their promotion on a benchmark. An action prompt could regress for months and the only evidence
was a vaguely worse result.

**Every run names its prompt.** The device reports `prompt_sha256` — sha256 of the *rendered*
prompt, params substituted, so it is the text the model actually read — plus the action's declared
`version` if it has one. Both land in the settle event's metadata beside `action_type`. The
fingerprint is computed before anything is spawned, so a posture refusal that costs nothing names
its prompt too; the one unstamped outcome is an action that could not be loaded at all, which had
no prompt to fingerprint.

**Content is opt-in, per action.** `report_io = true` in `action.toml` (off by default) also sends
the rendered prompt and the result text, which the cloud stores as the event's `input`/`output`.
That is the whole gate: `lt-runner score` and `score-traces` judge a relay run exactly when its
action opted in, with **no new scorer and no new table**. With it off the cloud holds the
fingerprint only — enough to see that a prompt changed on the 14th and the failures start on the
14th, without the text. This is the same promise the rest of this document makes: prompts and
results stay on the device unless the operator says otherwise.

The payload goes through the **same redaction door** every other ingest path uses
(`crates/api/src/redact.rs`), so an opted-in action's stored prompt is PII-scrubbed like any
captured payload. One exemption: `prompt_sha256` bypasses the scrub, because it is 64 hex
characters and the scrubber's "32+ hex is a secret" rule would collapse every fingerprint to the
same `<SECRET>` — the same reasoning that already exempts the `hash` persistence policy's digests.

| Route | Auth | Purpose |
|---|---|---|
| `GET /v1/relay/actions?project=&limit=` | project key (own) / admin | The fingerprint ledger, **derived from the settle events** — distinct `action_type × prompt_sha256` with `versions`, `runs`, `errors`, `judgeable` (how many carry content), and `first_seen`/`last_seen`. `limit` bounds how many events are walked (default 1000, cap 20000); the answer carries `scanned` and `truncated`, because a ledger that stopped early without saying so reads as "this action has one prompt" when it has three. |
| `POST /v1/relay/actions/:action_type/dataset` | admin | Snapshot the action's succeeded tasks (`payload → input`, `result → output`) into a dataset, so a benchmark can be linked and the next prompt edit is gated like a registry prompt's. Body: `{ project_id, name?, limit? }` (default 200, cap 1000). Answers the dataset plus `items` and `skipped`. The source is the **task**, not the settle event, so an action can be benchmark-gated without its prompt text ever reaching the cloud. The dataset is left unfrozen — freezing is the curator's call once they have looked at the cases. |

A namespaced `action_type` percent-encodes its `/` in the path:
`POST /v1/relay/actions/xprice%2Freprice-summary/dataset`.

## The device side (`crates/agent`, binary `lt-agent`)

Modules: `config` (agent.toml; device keys named by env var, never inlined), `cloud` (lease +
settle client per source), `actions` (library loading, `{{…}}` template rendering, `${ENV}`
header expansion, action-type path validation so a network-supplied name can never escape the
library), `exec` (run one task → `RunReport`), `connect` (result propagation), `run` (the loop).
Execution is serial and rotates across sources round-robin — one Claude run at a time respects
the machine and the subscription window, and one busy cloud can't starve the others.

There is deliberately **no local queue**: crash recovery is lease-based. If the agent dies mid-run,
the cloud reclaims the task once its lease expires. `lease_secs` no longer has to cover the longest
expected run — a heartbeat thread renews the lease at the `renew_secs` the cloud hands back, so the
TTL is detection latency and a Claude Code run takes as long as it takes. A renewal refused with
`409` stops the run and **suppresses the settle entirely**: the task belongs to someone else now, and
delivering its result through a connector is not something the cloud could undo.

Action library — gitignored except `actions/README.md` + `actions/_example/`
(see `actions/README.md` for the authoring guide):

```
actions/
  xprice/reprice-summary/
    prompt.md        # required — template with {{params.*}} / {{payload}} / {{task_id}}
    action.toml      # model, system, schema_file, posture (mode/workspace/tools/…), [connector]
    schema.json      # optional — result becomes schema-conforming JSON instead of text
```

### Posture: what a relay run is allowed to touch

This document always said allowed tools live on the device. Until the invocation seam landed the
library could not actually *say* so: every action ran as a plain completion whatever it needed.
Now each action declares a `mode`, and the engine's one seam (`lighttrack_engine::invocation`)
enforces it — a contradiction is an error **before** the CLI is spawned, so an over-claiming
action costs nothing rather than being discovered in a diff.

| `mode` | workspace | tools | `--permission-mode` | argv shape |
|---|---|---|---|---|
| `generate` (default) | forbidden; runs in a neutral temp dir, so no ambient `CLAUDE.md`/hooks join the prompt | none | forbidden | `-p --output-format json --model … [--effort] [--append-system-prompt] [--json-schema] [--bare]` |
| `readonly-scan` | **required** | base `Read`/`Glob`/`Grep`/`LS` + declared extras, each of which must be read-only | optional; `plan` or `default` only | the above, plus `--permission-mode`, `--max-budget-usd`, and `--allowedTools` **last** (it is variadic) |
| `edit` | **required** | declared list, no base set | **required** (e.g. `acceptEdits`) | as `readonly-scan` |

`workspace` is a name relative to `workspaces_root` in `agent.toml`, validated by the same rule as
`action_type` (no absolute path, no `..`, no backslashes) and required to exist. With
`workspaces_root` unset the device runs no scan or edit action at all: reaching a repository takes
an operator naming its parent directory, never a cloud payload. A `readonly-scan` that lists a
write-capable tool is rejected — anything the allowlist doesn't recognise counts as write-capable.

Two more properties the seam fixes for every caller: the **prompt travels over stdin**, not argv
(Windows caps a command line at ~32k characters and a quote-heavy judge prompt was fragile there),
and the **billing key is decided once** — a seat run strips `ANTHROPIC_API_KEY` from the child so
subscription work cannot silently bill the metered API, while `--bare` requires it. `lt-agent`
runs seat-authenticated, which is the whole point of the relay.

`http` POSTs the result envelope to the app's callback; `command` pipes it to a local script on
stdin (covers any database or bespoke API without LightTrack needing drivers). A connector
failure settles `failed` — the retry re-runs the action, which is why connectors must be
idempotent. On a rate-limit error from the CLI (`usage limit` / `429` / `overloaded` on stderr),
the agent settles `deferred` so the attempt is handed back.

Run it with `lt-agent --config agent.toml` (copy `agent.example.toml`); `--once` drains every
source and exits — useful for testing and cron-style scheduling. Note that subscription-auth CLI
calls carry Claude Code's own context overhead (~30k input tokens per run). It consumes window
capacity, and now that runs are priced from the envelope it shows up in the cost too — prefer
batching work into fewer, larger actions.

## Reuse across projects (xprice)

Don't duplicate the mechanism — the cloud LightTrack instance is the single broker. Each xprice
app is just a client: its own project API key, an action folder in the device's library under its
namespace (`actions/xprice/...`), and the SDK helpers. N devices, N apps: a task reaches whichever
enrolled device advertises its namespace, so `actions/xprice/*` can live on one machine and
`actions/ops/*` on another without either knowing about the other. If a project someday needs its
own broker, the agent's multi-source config already covers it.

```python
task = lt.relay_task("xprice/reprice-summary", {"sku": "A-1"}, idempotency_key="order-42")
task = lt.wait_relay_task(task["id"])            # optional: poll until succeeded | dead
```

```ts
const task = await lt.relayTask("xprice/reprice-summary", { payload: { sku: "A-1" } });
const done = await lt.waitRelayTask(task.id);    // optional
```

Unlike the fire-and-forget `track*` telemetry, relay calls are functional: they return the task
and **raise/throw** (`RelayError`) on failure. Prefer the connector push for delivery;
`wait_relay_task` is for tasks the device is expected to pick up promptly.

## Constraints & risks

- **Subscription terms.** Claude Code headless automation for your own apps is the intended
  gray-zone-safe use; serving external users at volume through a consumer subscription is not.
  Keep relay traffic owner-facing/batch.
- **Fleet availability** — no longer intrinsically single-device (M18): enrol several devices and a
  task goes to whichever advertises its action type. It is still eventually consistent, and a fleet
  where only one device carries a given capability is that capability's SPOF — which is now at least
  *visible*, in `GET /v1/relay/devices` and in the enqueue verdict's `eligible_devices`. Mitigated by
  the 20h retry envelope, dead-letter alerts, and `relay_task_unroutable`.
- **Payload privacy** — params rest in the cloud DB until executed, and secrets stay device-side by
  construction. Ingest redaction (`LIGHTTRACK_REDACT_INGEST`) covers the **run event** a device posts
  back, including its `error` string — but **not** the task `params` in `relay_tasks`, which are
  stored as submitted. Don't put anything in params you wouldn't want at rest in the cloud DB.

## Status

- **Phase 1 (shipped):** `relay_tasks` domain — core type, Store trait + SQLite impl, five API
  routes, device-key guard, lease/settle semantics, store + router tests.
- **Phase 2 (shipped):** `lt-agent` — multi-source round-robin lease loop, action library with
  template rendering + schema output, `http`/`command` connectors, deferred-on-rate-limit,
  cloud-side event logging on settle (then $1 flat; priced from the envelope since D18), `actions/`
  scaffolding + `agent.example.toml`. Smoke-verified end to end against the real Claude CLI.
- **Invocation seam (shipped):** every `claude -p` in the workspace runs through
  `lighttrack_engine::invocation` — one spawn site, one bin resolver, prompt over stdin, one
  billing-key decision, and posture enforced per `mode` (see the matrix above). Actions declare
  `mode`/`workspace`/`allowed_tools`/`permission_mode`/`max_budget_usd`/`timeout_secs`;
  `agent.toml` gains `workspaces_root`; `RunReport` and the settle body carry `cost_usd` + `mode`.
- **Phase 3 (shipped):** dead-letter alerts on both death paths (settle-exhaustion + pre-lease
  sweep, webhook-verified), long-poll lease (`wait_secs`, agent-configurable), Python
  `relay_task`/`get_relay_task`/`wait_relay_task` + TS `relayTask`/`getRelayTask`/`waitRelayTask`
  (both raise/throw `RelayError`).
- **Device fleet — M18 (shipped):** `devices` table + `Surface::Devices` on both SQLite and Postgres
  (Firestore refuses, conformance asserts it); hashed per-device `ltd_…` keys with three admin
  routes; leases filtered by advertised capabilities inside the claim; `lt-agent` advertising an
  inventory derived from its action library; enqueue answering `queued { eligible_devices }` or
  `422 relay_unroutable`; `relay_task_unroutable` on the M7 sweep; three read-only MCP tools;
  `lt relay devices`; and `RelayError.is_unroutable` in both SDKs. The legacy shared
  `LIGHTTRACK_RELAY_DEVICE_KEY` still works, unfiltered and deprecated, for one release.
- **Postgres (shipped):** all seven relay methods in `store-pg/src/relay.rs` + the
  `relay_tasks` table in `schema/postgres/001_init.sql`. Lease/sweep are single-statement
  `UPDATE … RETURNING` (lease adds `FOR UPDATE SKIP LOCKED`); settle wraps read-branch-update in
  one transaction with `SELECT … FOR UPDATE` so duplicate reports can't double-apply. Covered by
  the shared conformance suite (relay section skips backends without support) — CI's ephemeral
  Postgres runs it automatically — and smoke-verified over HTTP against Postgres 16.
