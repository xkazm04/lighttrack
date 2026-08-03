# LightTrack — Architecture

## 1. Goals & non-goals
**Goals:** headless-first LLM observability for 5–10 apps (~10–100 calls/hour each) across OpenAI /
Gemini / Anthropic; open, queryable data; per-project cost/limit tracking; LLM-as-judge scoring &
benchmarking; near-zero infra cost on GCP free tier; MCP access for Claude Code.

**Non-goals (for now):** multi-org SaaS, fine-grained RBAC, a bespoke web UI (use Looker Studio),
tracking LightTrack's own internal calls.

## 2. Data flow
```
 Monitored apps (local or cloud)                         LightTrack
 ────────────────────────────────                        ──────────
  OpenAI / Gemini / Anthropic SDK call
        │  (1) emit normalized event
        ▼            (thin SDK / HTTP POST / OTel GenAI)
   ┌──────────────────────────────┐   (2) auth(project API key), normalize, compute cost
   │  lighttrack-api  (axum)       │──▶ (3) write event ──▶  Store (SQLite local / BigQuery cloud)
   │  POST /v1/events              │   (4) update rolling counters, evaluate LimitRules
   │  GET  /v1/traces|costs|...    │        └─ breach ──▶ alert (Pub/Sub→Fn→email/Slack/ntfy)
   │  GET  /v1/limits/status       │                       + set advisory throttle flag
   └───────────┬──────────────────┘
               │ (5) enqueue scoring/benchmark jobs (Pub/Sub cloud / channel local)
               ▼
   ┌──────────────────────────────┐   (6) claude -p --output-format json --json-schema <verdict>
   │  lighttrack-runner            │──▶  THE LLM ENGINE (unbudgeted)
   │  pulls jobs, runs judge       │   (7) write Score rows back via API/Store
   └──────────────────────────────┘

   ┌──────────────────────────────┐
   │  lighttrack-mcp               │  read-mostly tools over the Store, for Claude Code / agents
   └──────────────────────────────┘
```

## 3. Components
| Crate | Role | Deploys to |
|---|---|---|
| `core` | Normalized `LlmEvent`, `PriceBook` + cost calc, `LimitRule` eval, scoring/benchmark types, `Store` trait (later). Pure, no I/O. | lib, used everywhere |
| `api` | Ingest + query REST (axum). API-key auth, cost computation, limit evaluation, job enqueue. | local box → **Cloud Run** |
| `runner` | Subscribes to jobs, invokes `claude -p`, parses JSON verdicts, writes scores. The judge. | local box → **e2-micro** |
| `mcp` | MCP server: `query_traces`, `get_cost_summary`, `list_projects`, `get_limit_status`, `run_benchmark`, … | wherever Claude Code runs |
| `cli` | Operator tool: query, manage projects/keys, define & trigger benchmarks. | anywhere |

## 4. Ingestion contract
Apps send a **normalized event** (see `docs/DATA_MODEL.md`). Two front doors, same internal model:
1. **`POST /v1/events`** — simple JSON, the default. A ~30-line client snippet per language wraps each
   provider call (record model, usage, latency, status) and posts the event. Cost is computed server-side.
2. **`POST /v1/traces`** — OTLP/HTTP **JSON** using the OTel GenAI semantic conventions; spans are
   mapped → events (see `docs/OTLP.md`). Keeps us vendor-neutral (the anti-lock-in lever vs Langfuse):
   an app already instrumented with OpenTelemetry needs no LightTrack SDK, just an exporter endpoint.
   Mapping is *all* this door does — the mapped events go through the same batch handler as (1), so
   validation, redaction, pricing and limit admission are identical. gRPC/protobuf OTLP and the
   metrics/logs signals are not accepted.

Provider SDKs already return token usage; the client just forwards it. Prompts/outputs are **optional**
and **redactable** per project (store nothing, hashes, or full text).

> Limits are enforced at **ingest admission**: a breaching event is rejected with HTTP 429 and not
> recorded, so cooperating clients back off. A future **gateway/proxy mode** (apps route calls *through*
> LightTrack) would additionally block the provider call *inline*, before the spend. Deferred — it adds
> latency and a critical-path dependency.

## 5. Storage — local→cloud parity
A `Store` trait abstracts persistence. Two backends:
- **Local (`v0`): SQLite** via `rusqlite` (bundled) — rock-solid on Windows, zero external services.
  Runs in **WAL** mode (asserted at open; an existing pre-WAL file is upgraded in place). Writes
  serialize behind one connection — admission control's check-then-insert depends on that — while
  reads are served from a small pool of read-only connections (`LIGHTTRACK_SQLITE_READ_POOL`,
  default 4, `0` disables), so a dashboard query no longer stalls ingest. WAL means the database is
  **three files**: `lt.db`, `lt.db-wal`, `lt.db-shm`. Back up or mount the *directory*, not the
  single file (or checkpoint first with `PRAGMA wal_checkpoint(TRUNCATE)`); the shipped Docker/Helm
  deployments already mount `/data` as a directory. If WAL can't engage (some network filesystems)
  the store logs a warning and falls back to routing reads through the write connection.
- **Cloud: BigQuery** for events/scores (the "do anything with the data" analytical store; 10 GB + 1 TB/mo
  query free) + **Firestore** for hot config (projects, keys, limit rules, counters).

Schemas are kept in lockstep (`schema/sqlite` ↔ `schema/bigquery`) so analytical queries port. DuckDB is a
drop-in local upgrade if we want columnar parity with BigQuery later.

## 6. Cost accounting
`PriceBook` (from `config/pricing.json`, keyed `"<provider>/<model>"`) → `cost_usd(provider, model, usage)`.
Cached-input tokens are billed at the cached rate when present. Events may carry a provider-reported cost
(e.g. Claude Code's `total_cost_usd`); otherwise we compute. Prices in the repo are **approximate — verify
against provider pricing pages** before trusting cost dashboards.

## 7. Limits (incoming traffic trips them; judge is exempt)
`LimitRule { project_id, metric: cost|calls|tokens, window: hour|day|month, threshold, action }`.
Ingest is **admission-controlled**: `POST /v1/events` evaluates the matching rules against rolling usage
*including the candidate event*. Every rolling window is measured on the server-stamped `received_at`,
never on the client's `ts` — otherwise one caller with a skewed clock could slide its spend outside the
window a cap is evaluated over. The client's `ts` is preserved verbatim (and still what listings, traces
and the cost rollups are windowed by); it is merely validated against a skew bound
(`LIGHTTRACK_MAX_TS_SKEW_FUTURE_SECS`, default 5 min; `LIGHTTRACK_MAX_TS_SKEW_PAST_SECS`, default 7 days;
`LIGHTTRACK_MAX_TS_SKEW_SECS=0` disables both), which rejects with the distinct codes `ts_too_new` /
`ts_too_old`. SQLite and Postgres both carry the column (Postgres via an additive migration that
backfills `received_at = ts`, with the reads coalescing either way). **Firestore** has not ported it:
there `received_at` reads back equal to `ts` and its windowed accounting is still ts-keyed.

The check and the insert are **one atomic store step**, so a concurrent burst can't all read the same
pre-burst usage and race past the cap (check-then-act TOCTOU). How each backend gets there — and where
it doesn't — is reported by `Store::admission_is_atomic()` and pinned by a conformance check that fires
eight simultaneous admissions at one cap:

| backend | mechanism | atomic |
|---|---|---|
| SQLite | one locked write connection + the usage cache held across check-count-insert; one transaction per batch | yes (single process per database file) |
| Postgres | one transaction per admission (per batch for the batch path), serialized per project by a transaction-scoped advisory lock taken as its first statement; each batch item in its own SAVEPOINT so a per-item `Conflict` can't abort the transaction | yes (across every API process sharing the database) |
| Firestore | usage is summed client-side from a document scan, which it cannot evaluate-and-write in one transaction — it uses the trait's explicitly-named `insert_event_checked_nonatomic` | **no — caps are advisory**, warned at startup and reported by the conformance suite |

Postgres takes a per-project advisory lock rather than `SERIALIZABLE` because admission is the ingest
hot path: under `SERIALIZABLE` a burst on one project would abort and retry itself repeatedly (each
retry re-reading the whole window), whereas the lock makes the waiters queue — the cost of contention
is latency on one project, never lost enforcement, and other projects are unaffected.

Actions are three genuinely different
tiers: **Alert** (notify only — the event is still recorded), **Throttle** (graduated — see §7c), and
**Block** (an unambiguous hard stop at the threshold). Both enforcing tiers reject with **429
`rate_limited`** and do *not* record the event, so a cooperating client backs off; the breach is also
readable via `GET /v1/limits/status` and MCP. Inline *pre-call* blocking (before the provider spend)
still requires gateway mode. The scoring/benchmark engine is **not** subject to limits.

### 7a. Unpriced traffic under a cost cap
An event whose model is absent from the price book stores `cost_usd = NULL` — never a phantom zero,
because that invariant is what makes margin/analytics honest. But `SUM(cost_usd)` reads `NULL` as
`0.00`, so a **cost cap used to be free to walk past on exactly the newest, least-vetted traffic**
(`cost_usd` is also the *default* limit metric). Fixed **inside the limit path only** — nothing is
written onto the event row, and there is no price discovery from providers:

- **Imputation.** Each unpriced call in a window is charged the mean cost of a *priced* call in the
  same window (`SUM(cost_usd) / priced_calls`). It uses only evidence already inside the window, and
  it self-corrects: as an operator adds the missing price, new traffic moves the mean.
- **Marked as estimated.** Every `cost_usd` status carries `cost_evidence`
  (`priced_calls`, `unpriced_calls`, `imputed_cost_usd`, `client_reported_cost_usd`, `unpriceable`).
  `current` includes the imputation — subtract `imputed_cost_usd` for the hard-evidence sum. A 429
  tripped on an imputed total says so in its message.
- **Unpriceable = refuse.** A window with unpriced calls and *no* priced call has nothing to impute
  from, so the cap cannot be measured at all. An **enforcing** rule rejects ingest in that state
  (429 `rate_limited`, message naming the price book), because a cap that cannot be measured is not a
  cap. `Alert` rules stay observe-only. `calls`/`tokens` caps are untouched by any of this.
- **No repricing of history.** `cost_usd` is stamped once at ingest, so correcting a *wrong* price-book
  entry does not restate spend already inside a window — the cap stays wrong until the window rolls.
  Only *unpriced* traffic self-corrects (its charge is computed at evaluation time). This absence is
  stated in `GET /v1/limits/status` → `cost_basis.notes`, not left to be discovered during an incident.
- **Client-reported cost** (`metadata.cost_source = "client"`) is summed separately and reported in
  `cost_evidence` / `cost_basis`, so an operator can see when a cap rests on the caller's own number.

Backend parity: SQLite and Postgres compute the provenance in SQL; Firestore folds it client-side.
Firestore reads `cost_source` out of the stored `metadata` JSON string.

### 7b. Load shedding (admission control for *load*, not spend)
Limit rules cap what a project may **spend**; nothing capped what the process may **attempt at once**.
Ingest requests queued behind the store's single lock with tokio's 512-thread blocking pool as the only
bound, so past saturation latency grew without limit and an operator could not tell a busy server from
a hung one. Ingest **POST** routes are now gated:

- at most `LIGHTTRACK_INGEST_MAX_INFLIGHT` (default 64, `0` = unbounded) requests run at once; one past
  that is rejected immediately — never queued — with **503 `overloaded`** and a `Retry-After`
  (`LIGHTTRACK_INGEST_RETRY_AFTER_SECS`, default 1);
- a request outliving `LIGHTTRACK_INGEST_TIMEOUT_SECS` (default 10, `0` = off) is cut with
  **504 `timeout`**;
- `GET /v1/ingest/status` reports live in-flight depth plus shed/timeout/admitted counters
  (process-local, reset on restart — the same honesty as the rejection ledger).

**`overloaded` (503) is never `rate_limited` (429).** 429 means *you* exceeded a configured budget and
the event was deliberately refused; 503 means the *server* is momentarily saturated and the identical
request will succeed shortly. A client confusing them would hammer a struggling server, or drop events
it was entitled to send.

Only the write path is gated, so an operator's reads — including `/v1/ingest/status` itself — stay
answerable while shedding. Shedding happens before the handler runs, so a shed request has touched no
store state. A *timeout* can fire while a handler awaits its blocking store call; dropping that future
does not cancel the `spawn_blocking` work, so the transaction still resolves on its own terms and the
client simply learns nothing about it — which is why ingest is replay-safe (resend the same event id;
a replay is acknowledged, never double-counted).

Measured on the in-process saturation harness
(`cargo test -p lighttrack-api -- --ignored shedding_bounds_latency_under_saturation`), offered load
300 → 600 → 1200 concurrent ingests: **unbounded**, served p95 157 → 182 → 741 ms (tracks offered
load); **cap=16**, served p95 24 → 65 → 91 ms, with shed responses at a flat p95 of ~0.13 ms whatever
the load.

### 7c. Throttle is a ramp, Block is a cliff
`Throttle` and `Block` used to be the same behavior under two names — both hard-rejected at the
threshold, nothing anywhere delayed or degraded. With a hard threshold and no hysteresis, traffic went
from fully accepted to fully rejected between two consecutive events, and the client's only warning
was a `ratio` it had to poll a second endpoint to see. `Throttle` is now **proportional shedding**:

- **Ramp.** Shedding starts at the rule's `warn_at` (reusing the operator's own "approaching" mark
  rather than adding a second knob that could contradict it), or `0.8` when unset, and rises linearly
  to 100% at the threshold. `shed_fraction` is reported on every status.
- **Per-event decision, deterministic.** An event is shed iff `FNV1a+splitmix64(rule_id, event_id)`
  lands under `shed_fraction`. No RNG: the same event always gets the same verdict, so behavior is
  reproducible and testable, and raising the pressure only ever *adds* events to the shed set — the
  ramp is monotone, so there is no flapping at the boundary. At exactly `throttle_start` the shed
  fraction is `0.0` and nothing is shed.
- **Why shedding rather than a delay or a queue.** Delaying holds a request (and a blocking-pool slot)
  open, which is how a budget control turns into a *load* problem — precisely what §7b exists to
  prevent. Shedding is O(1), keeps the server's own back-pressure independent of the client's, and a
  429 with `Retry-After` is a schedule a well-behaved client can honor without us holding state.
- **Block is untouched.** It sheds nothing before the threshold and is a hard stop at it.
- **Retry schedule.** Every limit 429 now carries `Retry-After`: 1–15s for a graduated shed (transient,
  grows with pressure), and the window's own back-off for a hard stop (`hour` 30s / `day` 300s /
  `month` 900s — nothing frees up faster than usage ages out). The batch path, which cannot carry a
  per-item header, returns `retry_after_secs` and `shed` on each rejected item.
- **Proximity on accepted writes.** `POST /v1/events` returns `usage_ratio` (worst ratio among the
  rules that applied) and `shed_fraction`, so a client learns it is approaching a cap from the response
  it already gets, not from a separate poll.
- **Attribution.** A shed event is recorded in the rejection ledger exactly like a hard rejection
  (`shedding` on the status names the rule), so `/v1/limits/status` → `rejected` stays complete.

**Budget shedding (429 `rate_limited`) is never load shedding (503 `overloaded`).** See §7b: 429 means
*you* are near or over a configured budget; 503 means the *server* is saturated and the identical
request will succeed shortly.

## 8. Scoring & benchmarking engine
- **Online scoring:** sample events → enqueue → runner runs a rubric prompt via
  `claude -p --output-format json --json-schema <JudgeVerdict>` → store `Score`.
- **Benchmark:** a `BenchmarkDefinition` (dataset of inputs [+expected], target, rubric, judge model) →
  run target → judge each output → aggregate a scorecard in the Store → track over time → alert on
  regression vs baseline.
- **Engine is pluggable** (`claude -p` → direct API → other provider) and **unbudgeted**. Default judge
  model **Haiku** for cost, escalate to Opus for hard rubrics. The judge's own spend is recorded as a
  `Score.cost_usd` so we can watch Agent-SDK-credit burn — but never throttled.

## 9. Security
- **API keys per project** for ingest (`Authorization: Bearer lt_<prefix>_<secret>`); only a salted hash is
  stored. An **admin key** guards management endpoints.
- **Local dev:** bind to `127.0.0.1`; auth can run in a relaxed `dev` mode.
- **e2-micro:** API keys enforced; TLS via Cloud Run (managed) or Caddy in front of the VM. Secrets live in
  **Secret Manager** (cloud) / a git-ignored `.env`/`*.local.toml` (local), never committed.

### Error envelope
Every non-2xx response is a stable, machine-readable JSON envelope so clients (CLI, MCP, SDKs) branch on a
code instead of string-matching prose:
```json
{ "error": { "code": "not_found", "message": "event 'x' not found" } }
```
`code` is a frozen identifier; `message` is human-facing and may change wording — never parse it. Codes and
their canonical HTTP status (see `crates/api/src/error.rs`):

| code | status | meaning |
|------|--------|---------|
| `bad_request`  | 400 | malformed / invalid request (validation) |
| `unauthorized` | 401 | missing or invalid credentials |
| `forbidden`    | 403 | authenticated but not permitted |
| `not_found`    | 404 | resource does not exist |
| `conflict`     | 409 | conflicts with current state (duplicate / frozen / gated regression) |
| `rate_limited` | 429 | ingest rejected: an enforcing (`throttle`/`block`) limit was breached (see §7) |
| `internal`     | 500 | unexpected server fault (store / serialization / I/O) |

Store-layer failures all collapse to `internal` — clients must not branch on backend internals.

## 10. Notifications
Cloud Scheduler (3 free jobs) fires periodic checks (rolling cost, score regression) → Pub/Sub → Cloud
Function → email (SendGrid/Gmail) / Slack webhook / ntfy. Plus inline limit-breach alerts from `api`, and
native GCP budget alerts for infra spend.

## 11. Deployment
- **Phase A (now): local.** `cargo run` for `api` + `runner`; SQLite file; `claude -p` on this machine.
- **Phase B: GCP.** `api`→Cloud Run (container, scales to zero), `runner`→e2-micro (orchestrates remote
  `claude -p`), BigQuery + Firestore, Pub/Sub, Cloud Scheduler, Secret Manager. Looker Studio on BigQuery.

See `docs/ROADMAP.md` for sequencing and `docs/DECISIONS.md` for the rationale behind each choice.
