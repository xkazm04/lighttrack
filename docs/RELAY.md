# RELAY — cloud→device task queue for local Claude Code execution

Deployed apps enqueue heavy, offline-tolerant LLM tasks on the cloud LightTrack instance; one
enrolled local device (running the user's Claude Code subscription) leases them over outbound
HTTPS, executes them against a local action library, pushes results back into the apps, and logs
every run to LightTrack. The point: route heavy LLM work that doesn't need an online reaction
through the flat-rate Claude subscription instead of metered APIs, while the Gemini production
engine keeps serving the latency-sensitive paths.

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
                                        │ outbound lease / result (LIGHTTRACK_RELAY_DEVICE_KEY)
                              lt-agent (local device)
                                 │ actions/<type>/  ← gitignored: prompt.md + action.toml + connector
                                 │ claude.exe -p … --output-format json  (engine::invocation::run)
                                 │ connector (http | command) → pushes result into the app
                                 └─ POST /v1/relay/tasks/:id/result (+ usage → cloud logs $1 event)
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
| `POST /v1/relay/tasks` | project key | Enqueue `action_type` + `payload` (+ `idempotency_key`, `max_attempts`, `retry_interval_secs`, `source`). A project key is forced into its own project. |
| `GET /v1/relay/tasks/:id` | project key (own) / admin | Status + result — the originating app's polling fallback. |
| `GET /v1/relay/tasks?project=&status=&limit=` | project key (own) / admin | List/inspect. |
| `POST /v1/relay/lease` | device key | Lease up to `max` due tasks for `device`, holding each for `lease_secs` (60s–1800s). Optional `wait_secs` (≤25) long-polls until a task is due. Answers `{ tasks, lease_secs, renew_secs }` — the granted TTL after clamping, and how often to renew. |
| `POST /v1/relay/tasks/:id/renew` | device key | Heartbeat, carrying `fence`. `409` = the lease is no longer yours: stop, and do not deliver. |
| `POST /v1/relay/tasks/:id/progress` | device key | Liveness detail (`fence` + `progress`), visible on the task. Deliberately not on the heartbeat. |
| `POST /v1/relay/tasks/:id/cancel` | project key (own) / admin | Stop a queued or leased task. `409` if it already finished. |
| `POST /v1/relay/tasks/:id/result` | device key | Settle: `succeeded` (+`result`) \| `failed` (+`error`) \| `deferred` (+`retry_after_secs`), carrying `fence`. Optional usage/accounting: `model`, `input_tokens`, `output_tokens`, `latency_ms`, `cost_usd`, `mode`. `409` = not held; the result was NOT recorded. |

Device enrollment is deliberately minimal for the single-device case: set
`LIGHTTRACK_RELAY_DEVICE_KEY` on the cloud instance (Secret Manager on Cloud Run) and give the
same secret to `lt-agent`. No key-minting endpoint exists — nothing to leak over MCP; multi-device
enrollment (hashed keys in a table, like API keys) is future work if ever needed. The admin key
(and dev mode) also passes the device guard, for local testing.

Store: `relay_tasks` on the `Store` trait, declared as `Surface::Relay` in the capability manifest
(`docs/PARITY.md`). SQLite is the reference implementation; **Postgres implements the full domain**
(`store-pg/src/relay.rs` + `relay_lease.rs` — `FOR UPDATE SKIP LOCKED` leases, transactional fenced
settle), so the Neon-backed cloud serves relay natively. Firestore does not declare the surface, so
every relay method there answers `501 unsupported` and the conformance suite asserts that refusal.

The fence is proved, not assumed: the shared conformance suite's `relay_lease` section drives a
device to lease, renew, report progress, lose its lease to a reclaim, and then settle — and asserts
the late settle is `NotHeld` with the successor's task untouched. There is no way to observe that
race from outside, which is exactly why it is pinned there.

## Cost model: $1 flat per request

Billing credits remain what they were designed for — the Gemini production engine. Relay runs
are subscription-covered, so LightTrack tracks them at a **fixed $1.00 per executed request**.
The **cloud logs the event itself on settle** (no project key needed on the device, one writer):
a terminal `succeeded`/`failed` report on a live lease inserts an `LlmEvent` with
`cost_usd = LIGHTTRACK_RELAY_FLAT_COST_USD` (default 1.0), `provider: "anthropic"`, the
`source`/tokens/latency the device reported, `trace_id = task_id` (retries of one task group
into one trace), and `metadata: { task_id, action_type, attempt, device_cost_usd, mode }`.
`deferred` logs nothing — no run happened.

The device now reports what the CLI envelope said the run cost (`cost_usd`) and the posture it
ran under (`mode`); both land in that metadata as **evidence, not a bill**. The stamped
`cost_usd` stays the flat price — switching relay runs to envelope or token pricing is its own
decision, and making it a side effect of reporting would move every margin number without anyone
asking. Not precise, but a solid usage overview from day one; once the apps get
traction, switch to token-priced costing from the DB price book — the tokens are already
recorded, only the stamped `cost_usd` changes.

Relay events are always recorded (plain insert, not admission-checked): enforcing limits exist
to cap metered spend, and the run has already happened on the flat-rate subscription. They still
show up in costs, usage and forecasts like any other traffic.

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
calls carry Claude Code's own context overhead (~30k input tokens per run); irrelevant to cost
on flat rate, but it consumes window capacity — prefer batching work into fewer, larger actions.

## Reuse across projects (xprice)

Don't duplicate the mechanism — the cloud LightTrack instance is the single broker. Each xprice
app is just a client: its own project API key, an action folder in the device's library under its
namespace (`actions/xprice/...`), and the SDK helpers. One device, one agent, N apps. If a
project someday needs its own broker, the agent's multi-source config already covers it.

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
- **Single-device SPOF** — intrinsic; mitigated by the 20h retry envelope and dead-letter alerts.
  Apps must treat relay results as eventually consistent.
- **Payload privacy** — params rest in the cloud DB until executed, and secrets stay device-side by
  construction. Ingest redaction (`LIGHTTRACK_REDACT_INGEST`) covers the **run event** a device posts
  back, including its `error` string — but **not** the task `params` in `relay_tasks`, which are
  stored as submitted. Don't put anything in params you wouldn't want at rest in the cloud DB.

## Status

- **Phase 1 (shipped):** `relay_tasks` domain — core type, Store trait + SQLite impl, five API
  routes, device-key guard, lease/settle semantics, store + router tests.
- **Phase 2 (shipped):** `lt-agent` — multi-source round-robin lease loop, action library with
  template rendering + schema output, `http`/`command` connectors, deferred-on-rate-limit,
  cloud-side $1-flat event logging on settle (`LIGHTTRACK_RELAY_FLAT_COST_USD`), `actions/`
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
- **Postgres (shipped):** all seven relay methods in `store-pg/src/relay.rs` + the
  `relay_tasks` table in `schema/postgres/001_init.sql`. Lease/sweep are single-statement
  `UPDATE … RETURNING` (lease adds `FOR UPDATE SKIP LOCKED`); settle wraps read-branch-update in
  one transaction with `SELECT … FOR UPDATE` so duplicate reports can't double-apply. Covered by
  the shared conformance suite (relay section skips backends without support) — CI's ephemeral
  Postgres runs it automatically — and smoke-verified over HTTP against Postgres 16.
