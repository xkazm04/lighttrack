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
`ts_too_old`. Postgres and Firestore have not ported the column yet: there `received_at` reads back equal
to `ts` and their windowed accounting is still ts-keyed.

The check and the insert are **one atomic store step**, so a concurrent burst can't all read the same
pre-burst usage and race past the cap (check-then-act TOCTOU). Actions: **Alert**
(notify only — the event is still recorded), **Throttle** and **Block** (both **enforced** — a breaching
event is rejected with **429 `rate_limited`** and *not* recorded, so a cooperating client backs off; the
breach is also readable via `GET /v1/limits/status` and MCP). Inline *pre-call* blocking (before the
provider spend) still requires gateway mode. The scoring/benchmark engine is **not** subject to limits.

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
