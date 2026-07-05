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
                                 │ claude.exe -p … --output-format json   (engine::run_raw)
                                 │ connector (http | command) → pushes result into the app
                                 └─ POST /v1/relay/tasks/:id/result (+ usage → cloud logs $1 event)
```

## Task lifecycle

Statuses: `queued → leased → succeeded | dead`. A failed attempt goes back to `queued` with the
error recorded and `next_attempt_at` pushed out; exhausting `max_attempts` flips it to `dead`.
Dead-lettering **alerts** through the existing channels (`LIGHTTRACK_ALERT_WEBHOOK` /
`LIGHTTRACK_ALERT_NTFY`, event `relay_task_dead` — see `docs/ALERTS.md`), from both paths: a
failure report that exhausts the attempts, and the pre-lease sweep that catches vanished devices.

- **Retry policy** (per task, defaults): `max_attempts = 4`, `retry_interval_secs = 18000` (5h —
  one Claude subscription usage window). A fully offline device therefore has a ~20h envelope
  before tasks dead-letter.
- **Attempts are consumed on lease**, so a device that vanishes mid-run can't loop a task
  forever: an expired lease is re-leasable while attempts remain, dead-lettered once exhausted.
- **`deferred` hands the attempt back.** When the device can't attempt at all (subscription
  window exhausted, weekly cap), it settles `deferred` with an optional `retry_after_secs`.
  Rate limits never burn one of the 4 real attempts.
- **Duplicate result reports are harmless**: settling a task that is no longer leased returns it
  unchanged. Delivery is at-least-once end to end, so connectors must be idempotent — the
  `idempotency_key` is carried on the task for exactly that purpose, and re-enqueueing with the
  same key returns the existing task instead of a duplicate.

## API surface (Phase 1 — shipped)

| Route | Auth | Purpose |
|---|---|---|
| `POST /v1/relay/tasks` | project key | Enqueue `action_type` + `payload` (+ `idempotency_key`, `max_attempts`, `retry_interval_secs`, `source`). A project key is forced into its own project. |
| `GET /v1/relay/tasks/:id` | project key (own) / admin | Status + result — the originating app's polling fallback. |
| `GET /v1/relay/tasks?project=&status=&limit=` | project key (own) / admin | List/inspect. |
| `POST /v1/relay/lease` | device key | Lease up to `max` due tasks for `device`, holding each for `lease_secs` (60s–6h). Optional `wait_secs` (≤25) long-polls until a task is due. |
| `POST /v1/relay/tasks/:id/result` | device key | Settle: `succeeded` (+`result`) \| `failed` (+`error`) \| `deferred` (+`retry_after_secs`). |

Device enrollment is deliberately minimal for the single-device case: set
`LIGHTTRACK_RELAY_DEVICE_KEY` on the cloud instance (Secret Manager on Cloud Run) and give the
same secret to `lt-agent`. No key-minting endpoint exists — nothing to leak over MCP; multi-device
enrollment (hashed keys in a table, like API keys) is future work if ever needed. The admin key
(and dev mode) also passes the device guard, for local testing.

Store: `relay_tasks` on the `Store` trait with default "unsupported"/empty impls. SQLite is the
reference implementation; **Postgres implements the full domain** (`store-pg/src/relay.rs` —
`FOR UPDATE SKIP LOCKED` leases, transactional settle), so the Neon-backed cloud serves relay
natively. Firestore stays on defaults until needed (its conformance skips the relay section).

## Cost model: $1 flat per request

Billing credits remain what they were designed for — the Gemini production engine. Relay runs
are subscription-covered, so LightTrack tracks them at a **fixed $1.00 per executed request**.
The **cloud logs the event itself on settle** (no project key needed on the device, one writer):
a terminal `succeeded`/`failed` report on a live lease inserts an `LlmEvent` with
`cost_usd = LIGHTTRACK_RELAY_FLAT_COST_USD` (default 1.0), `provider: "anthropic"`, the
`source`/tokens/latency the device reported, `trace_id = task_id` (retries of one task group
into one trace), and `metadata: { task_id, action_type, attempt }`. `deferred` logs nothing —
no run happened. Not precise, but a solid usage overview from day one; once the apps get
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

There is deliberately **no local queue**: crash recovery is lease-based. If the agent dies
mid-run, the cloud reclaims the task when its lease expires and the retry consumes an attempt.
`lease_secs` (default 1800) must cover the longest expected run.

Action library — gitignored except `actions/README.md` + `actions/_example/`
(see `actions/README.md` for the authoring guide):

```
actions/
  xprice/reprice-summary/
    prompt.md        # required — template with {{params.*}} / {{payload}} / {{task_id}}
    action.toml      # model = "sonnet@high", system, schema_file, [connector]
    schema.json      # optional — result becomes schema-conforming JSON instead of text
```

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
- **Payload privacy** — params rest in the cloud DB until executed; ingest redaction
  (`LIGHTTRACK_REDACT_INGEST`) applies, and secrets stay device-side by construction.

## Status

- **Phase 1 (shipped):** `relay_tasks` domain — core type, Store trait + SQLite impl, five API
  routes, device-key guard, lease/settle semantics, store + router tests.
- **Phase 2 (shipped):** `lt-agent` — multi-source round-robin lease loop, action library with
  template rendering + schema output, `http`/`command` connectors, deferred-on-rate-limit,
  cloud-side $1-flat event logging on settle (`LIGHTTRACK_RELAY_FLAT_COST_USD`), public
  `engine::run_raw` / `resolve_claude_bin`, `actions/` scaffolding + `agent.example.toml`.
  Smoke-verified end to end against the real Claude CLI.
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
