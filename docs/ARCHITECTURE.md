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

### 7a1. A threshold can be derived, and a rule can be written by the system
A `threshold` is a number **or** `{"pct": N, "dimension": "customer"}` — a share of the subject's
recognized revenue, resolved at *evaluation* time against the same recognition the `/v1/margin`
rollup uses, so a cap follows the invoice instead of going stale on it. Every `LimitStatus` carries a
`basis` naming what the number came from, and the 429 says it in words. A derived threshold whose
revenue cannot be measured resolves to `+inf` and never breaches (`basis.kind = "unknown"`): a
guardrail we cannot measure is inert by design, never a guess that could become a surprise 429.

Rules therefore have **provenance**. `origin` records what created a rule when it was not a human
(`margin_policy:<policy id>:<subject>`), `expires_at` when a machine-made rule lapses, and
`escalated_until` when a forecast-driven tightening reverses. Three consequences an operator can rely
on, each pinned by a test:

- The forecast sweep is the **only** writer of these fields, and it only ever touches rules carrying
  its own `origin`. A hand-made cap is untouchable by automation.
- Escalation *shadows* `action` rather than overwriting it, so de-escalation is a field clear and not
  a remembered undo — and a sweep that stops running cannot leave a project throttled, because the
  lapse is on the row.
- An expired rule is inert at evaluation, sweep or no sweep.

`docs/MARGIN.md` ("Guardrails") has the policy vocabulary and the full behaviour.

### 7a0. Scoped rules and per-key budgets
A rule's optional `scope` narrows it to one value of one dimension:
`{"provider":…}` · `{"model":…}` · `{"name":…}` (use-case) · `{"api_key":…}` · `{"customer":…}`.
An unscoped rule is project-wide (the original behavior). A scoped rule *only applies to* matching
traffic: a non-matching event never counts toward it and can never be rejected by it, and its rolling
usage is read over its own dimension slice. A row carrying no value on the dimension (an unnamed call,
an untagged customer) matches no scope on it.

**Per-key budgets.** `{"api_key": "<key-id>"}` is what lets ten keys of one project have ten different
budgets — staging at $5/day, production at $500. The value is the opaque `api_keys.id`: never the key
material, never its prefix, never a hash of the secret. Ingest stamps it onto the event as
`metadata.api_key_id`, server-side from the authenticated principal, **overwriting or removing whatever
the body contained** — same trust rule as `received_at`. Without that strip, a caller could bill its
spend to another key or dodge its own cap by claiming a different id. Admin/dev principals are not keys,
so their traffic is deliberately *unattributed* rather than borrowing an identity. Events written before
this existed carry no `api_key_id` and fall into the unattributed bucket.

The linkage rides in `metadata` (like `customer_id` / `cost_source`) rather than a column, so it needed
no migration on any backend; SQLite/Postgres read it with `json_extract` / `->>`, Firestore parses the
stored JSON string client-side.

**The failure class — `metadata.failure_class`.** The single most consequential fact about a failed call
is whether it is **transient** (the provider or the network faltered; waiting fixes it) or **terminal**
(it will fail identically on every retry; a person has to look). `crates/engine/src/retry.rs` states the
workspace rule — *"Classification is by typed `EngineError` variant — never by string-matching provider
messages"* — and it holds inside the engine, which still has the structured response. It could not hold
in `crates/responder`, which classifies an error it did not produce, arriving as an `Option<String>` that
crossed a process boundary with no variant left to match on. A second, prose-reading classifier grew
there by necessity, and the two disagree by construction: `EngineError::is_retryable()` matches three
variants; `classify.rs` matches eleven phrases and six status codes, and nothing compared them.

So the class is minted where the structure still exists — in the caller's process, by the SDK holding the
provider's status code — and **carried**:

| stage | what happens |
| --- | --- |
| producer (SDK / any client) | sends `metadata.failure_class` = `transient` \| `terminal` on a failed call |
| ingest (`crates/api/src/events.rs`) | accepts it, **validates** it against the closed vocabulary, and defaults it to `unknown` when absent. It is never *inferred* here from `error` — that inference is the fallback's job, one layer down. A success carries no class, and any the client sent is removed |
| alert payload (`crates/api/src/alerts/channels.rs`) | `spike.failure_class` rides the `error_spike` webhook |
| responder (`crates/responder/src/classify.rs`) | `decide()` reads the carried class first; `classify()` — the phrase list — runs **only** for `unknown` |

Three states, and the third is not decoration: `terminal` goes straight to an investigation *without*
consulting the message (so a real bug whose text happens to say "timed out" is still diagnosed), while
`unknown` is the only input that reaches the phrase list at all. A value outside the vocabulary is
quarantined to `unknown` rather than passed through, so a client cannot invent a class a downstream
`match` has no arm for.

The phrase list is not deleted, and it is not a defect: it is the correct handling for a record whose
producer said nothing — an older SDK, a third-party producer, an OTLP export. What changed is that it is
no longer the *primary* path and its scope is stateable. Every use of it is counted
(`pipeline::CLASSIFIED`, `fallback_rate()`), so "how often are we still guessing" is a number.

**Not yet done:** no shipped SDK *mints* the class from the provider's response. The contract above is
open to any producer today (metadata is free-form), but until `clients/{rust,python,typescript}` set it,
the dominant ingest mix will be `unknown` and the phrase list will remain the real classifier. That is
the next piece of this, and it is a per-SDK change rather than a schema one.

**Who is spending — `GET /v1/limits/usage?project=&by=&window=&limit=`.** Rolling usage grouped by one
dimension (`api_key` default, or `customer`/`model`/`provider`/`name`), ranked by cost, each row
carrying the scoped rules that bind *that* value evaluated against *that* value's usage. This answers
"which key is burning the budget" **before** any rule exists (the sizing question) and "which key drove
this breach" after — over the authenticated API, not only inside an alert payload. Untagged traffic is a
`null`-valued row, so the parts sum to the project total. Admin callers additionally get a `label`
(key name + non-secret prefix); a project key gets ids only — a key is not handed a roster of its
siblings. Backends without the grouped query answer **501 `unsupported`**, never an empty breakdown.

Alert payloads deliberately do *not* enumerate key/customer contributors: a webhook fans out further
than the API does. A key- or customer-scoped breach alert states the operator's own rule scope and
points at `/v1/limits/usage`. The dimension is not exposed over MCP.

### 7a. Unpriced traffic under a cost cap
An event whose model is absent from the price book stores `cost_usd = NULL` — never a phantom zero,
because that invariant is what makes margin/analytics honest. But `SUM(cost_usd)` reads `NULL` as
`0.00`, so a **cost cap used to be free to walk past on exactly the newest, least-vetted traffic**
(`cost_usd` is also the *default* limit metric). Fixed **inside the limit path** — there is still no
price discovery from providers, and nothing is written onto the event row *at ingest*; M26 added one
explicit, operator-initiated exception (the forward fill below):

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
- **Seeing the gap, and closing it (M26).** `GET /v1/costs/unpriced` lists the `(provider, model)`
  pairs carrying unpriced traffic, ranked by calls, with the price book's own freshness beside them —
  before this, nothing said *which* models were missing, so the only symptom was a cost number that
  felt low. `PUT /v1/prices/:provider/:model?fill_unpriced=1` prices the stored `cost_usd IS NULL`
  rows for that key and answers `{filled, remaining_unpriced}`. The fill is opt-in, never automatic;
  it touches only rows that were never costed, so the no-retroactive-repricing rule above is intact;
  and every row it writes is stamped `metadata.cost_source = "book_fill"` + `priced_at`, so a cost
  reconstructed later stays distinguishable from one that was right at the time. See
  `docs/PRICING.md`.
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
- **The same signal on every ingest door, in headers.** The body could only ever carry it on
  `/v1/events`: `/v1/events/batch` answers multi-status (the project's position is not a property of
  item 7) and `/v1/traces` answers in the OTLP envelope, whose shape is the exporter's. So all three
  doors — and the 429 itself, which is the response that needs it most and has no `IngestResponse`
  body — also return `X-LightTrack-Usage-Ratio`, `X-LightTrack-Shed-Fraction` and
  `X-LightTrack-Retry-After`. Ratios are exact six-decimal strings; an **absent header means
  unknown, never `0`** (a project with no limits sends none at all). The batch door folds the worst
  of each axis, and the longest wait, across its items. `X-LightTrack-Retry-After` mirrors
  `Retry-After` — it exists because proxies and browser fetch stacks routinely strip or rewrite the
  standard header, and a back-off schedule that survives only some hops is not a schedule.
- **Which rule is binding.** `usage_ratio: 0.94` is only actionable if you know what is at 94%.
  `POST /v1/events` therefore also returns `binding_scope` (`{"kind":"model","value":"gpt-4o"}`, or
  omitted when the binding rule is project-wide) and `binding_rule` (its id). A project-wide cap
  means stop; a `model`-scoped one means route the next call elsewhere; a `name`-scoped one means
  only that call site pauses. `binding_rule` is what lets an SDK reproduce the server's own shed
  verdict — the decision is a hash of `(rule_id, event_id)`, so without the rule's identity a client
  can run the same function but never reach the same answer.
- **Pre-spend admission is the client's half.** The server's caps are record-side: they refuse to
  *record* a call that already cost money. The SDKs close that gap locally — each keeps the last
  limit view per (project, key, binding scope), and `admit()` answers from it with no I/O, so a call
  that would be refused is never made. `enforce: "block"` short-circuits the provider call with a
  typed budget error; `"warn"` logs and proceeds; `"off"` (the default) only observes. A locally
  blocked call is not spend and is not recorded as one — with `record_blocked` the SDK emits a
  zero-usage event tagged `lt_blocked_locally` so the rollups show the refusal without inventing
  cost. See `clients/README.md` and `clients/contract/fixtures/limits.json`, which fixes the
  admission verdicts in all three languages.
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
- **Scoped keys.** A key carries a set of capabilities — `ingest` (the event / batch / OTLP doors),
  `read` (the project's GETs), `manage` (its configuration writes). Three capabilities on a key, **not
  RBAC**: no roles, no inheritance, no per-resource grants — that non-goal still stands. What it buys is
  the case that used to be unaddressable: an ingest key embedded in a shipped client app could read back
  every prompt and completion stored for its project, because a project key had exactly one shape.
  `POST /v1/projects/:id/keys {"scopes": ["ingest"]}` (or `lt keys create --scope ingest`) fixes that.
  A key minted before scopes existed reads as `["ingest","read"]` — the permissive back-compat default
  for **one release**; the documented next default is `["ingest"]`.
- **Key expiry and rotation.** A key may carry `expires_at`; past it, it authenticates as nothing
  (401 `key_expired`). `POST /v1/projects/:id/keys/:kid/rotate {"grace_secs": 3600}` mints a successor with
  the same name and scopes and stamps the predecessor's expiry, so a fleet gets a window to redeploy
  instead of a cliff. The window is durable state on the row, not a background task a restart would drop.
- **The project switch is real.** `enabled = false` is checked when the key is verified, so a disabled
  project's keys open nothing — not ingest, not reads (403 `project_disabled`). Admin principals are
  unaffected, so an operator can always re-enable it.
- **`DELETE /v1/projects/:id` archives, it does not delete.** It sets `enabled = false` and stamps
  `archived_at`; the events, scores and benchmark runs stay, because they are what every cost report and
  gate decision was computed from. Archiving is idempotent, and effective — the tenant stops accepting work.
- **Tenant scope is a typed parameter on every store read (M17).** A `Store` read that names a
  project-bearing row takes a `Scope` — `Scope::Project(id)` for a project key, `Scope::Operator` for
  admin/dev and for the background sweeps — and the backend puts that filter *in the query*. The
  handler no longer reads a row and then compares its `project_id`, because that comparison could
  only produce a **403 that confirms the id exists**. D13 established this for traces; it now holds
  for events, scores, benchmarks and their runs, datasets and their items, rubrics, jobs, limit
  rules, margin policies, schedules, prompts and their versions, relay tasks, devices, alerts,
  alert channels and labels.
  - *Behavior change:* reading, updating or deleting another project's row is **404, not 403**, on
    every one of those endpoints. `GET /v1/benchmarks/:id`, `/v1/datasets/:id`, `/v1/rubrics/:id`,
    `/v1/relay/tasks/:id` and `/v1/relay/tasks/:id/cancel` previously answered 403 and now answer
    404 with the same `not_found` envelope. Clients that branched on 403 to mean "exists, not
    yours" were reading an oracle, not an API.
  - The one exception is the price book (`list_prices` / `upsert_price` / `list_price_history`):
    one rate card belongs to the instance, not to a tenant, and a per-project book would fragment
    every cost number computed from it.
  - `jobs` carries a nullable `project_id`, stamped at enqueue from the benchmark or schedule the
    work belongs to. `NULL` is an operator job (a sweep, or a row written before the column
    existed): the operator scope reads those and a project scope never does.
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
| `key_expired`  | 401 | a valid key that is past its `expires_at` — rotate it, do not retry |
| `forbidden`    | 403 | authenticated but not permitted (includes a key missing the route's scope) |
| `project_disabled` | 403 | the key is fine, but its project is disabled — re-enable the project |
| `not_found`    | 404 | resource does not exist |
| `conflict`     | 409 | conflicts with current state (duplicate / frozen / gated regression) |
| `rate_limited` | 429 | ingest rejected: an enforcing (`throttle`/`block`) limit was breached (see §7) |
| `internal`     | 500 | unexpected server fault (store / serialization / I/O) |

Store-layer failures all collapse to `internal` — clients must not branch on backend internals.

## 10. Notifications
Cloud Scheduler (3 free jobs) fires periodic checks (rolling cost, score regression) → Pub/Sub → Cloud
Function → email (SendGrid/Gmail) / Slack webhook / ntfy. Plus inline limit-breach alerts from `api`, and
native GCP budget alerts for infra spend.

### 10a. Pre-emptive forecast alerts fire on a schedule
`GET /v1/forecast` projects the daily counters forward: budget-breach ETAs ("you cross this cap in ~2
days") and margin erosion. That math was only ever reached from inside the handler, so a warning about
next week reached an operator only if they happened to look this week.

`forecast_sweep` closes it: a detached task in the **API process** walks the enabled projects on a
timer and hands the alerts to the same `Alerter`, with no HTTP request anywhere in the path
(`forecast::compute_forecast` takes no principal, no `Query`, no `Json` — the handler and the sweep
call the identical function).

- **API, not runner.** The runner's `recurrence` sweep is the right *shape* and this borrows it, but
  the alert cooldown map, the channel config and the `Store` handle all live in `AppState`, and the
  runner is an optional companion — a Cloud Run deployment ships the API alone. Hosting it there would
  mean the alerts silently don't fire in the deployment that most needs them, and would give it a
  second cooldown map that can't see the handler's.
- **Off by default.** `LIGHTTRACK_FORECAST_SWEEP_SECS` unset or `0` ⇒ no sweep; pull-only remains a
  supported stance, and turning a self-hosted process into an outbound notifier is a decision, not an
  inheritance. Floor 60s (a multi-day projection resampled faster than that is pure waste).
  `LIGHTTRACK_FORECAST_SWEEP_HORIZON` / `_LOOKBACK` shape the projection (default 14/14 days). The
  startup banner states which of these is in force.
- **No new dedup surface.** Alerts carry the existing `forecast:<project>:<kind>:<subject>` key, which
  says nothing about what triggered the forecast — so a sweep shares its cooldown with the handler and
  enabling it cannot double an operator's volume.
- **It cannot reach the ingest hot path.** Detached task; every store read on the blocking pool via
  `spawn_db`; yields between projects; a project that errors is logged and skipped rather than ending
  the loop.

## 11. Deployment
- **Phase A (now): local.** `cargo run` for `api` + `runner`; SQLite file; `claude -p` on this machine.
- **Phase B: GCP.** `api`→Cloud Run (container, scales to zero), `runner`→e2-micro (orchestrates remote
  `claude -p`), BigQuery + Firestore, Pub/Sub, Cloud Scheduler, Secret Manager. Looker Studio on BigQuery.

See `docs/ROADMAP.md` for sequencing and `docs/DECISIONS.md` for the rationale behind each choice.
Disk, retention and store maintenance are §12 below.

## 12. Disk: accounting, retention, and quiet-window maintenance

**Retention is deliberately unbounded — operator decision, 2026-08-24.** Nothing in this product
deletes an event, a score, a job or a revenue row, at any age. There is no pruner, no age-floor sweep
over the growing tables, and no delete-by-default policy, because keeping the data *is* the policy
for now: the production timeline and the audience are unresolved, and a retention default that
deleted would strand history on upgrade. The single exception is `collective_entries`, which a hub
prunes past an age floor — those are *other instances'* contributions, not this instance's own
history.

That decision is a real cost, and this section exists so it is a cost somebody can see. Disk grows
monotonically with ingest. Revisit retention when productionization resolves; until then the
obligation engineering carries is that the growth is measured, stated, and never surprising.

### What is measured — `GET /v1/storage/status` (admin)

One surface, three things an operator would otherwise guess at:

- **Per-object accounting.** Every table *and every index* as its own row: row count, bytes, share of
  the file, largest first. "The database is 2 GB" triggers panic; "one table is 1.7 GB of it"
  triggers a fix. Bytes come from SQLite's own page accounting (`dbstat.pgsize`) and every figure
  carries that predicate — `pgsize` is **pages allocated**, free space inside them included, which is
  *not* the same claim as bytes of live rows. The two diverge by exactly the reclaimable space, which
  is what lets the report answer its own follow-up question ("will anything shrink the file?"). Where
  the engine cannot be asked, bytes are `null` with the reason, never a measured-looking zero.
- **The journal sidecar**, from the filesystem — the engine's page accounting cannot see it, and it
  is a real part of what the store costs on disk. (WAL means the database is three files; back up the
  directory. See §5.)
- **The store's own latency**, keyed by operation family (`events.write`, `usage.read`,
  `pool.acquire`, …). The accounting says which table is *big*, the metrics say which family is
  *slow*, and the join of the two is the strongest prune-or-index signal the product can produce
  about itself. Slow counts always travel with the threshold they crossed.
- **The maintenance flight recorder** — every pass, *including every deferral*.

### What maintenance does, and what it will never do

Two acts, both **lossless**:

- **Checkpoint** the write-ahead journal (passive; truncating once the sidecar passes 8 MiB), moving
  already-committed pages into the database file.
- **Incremental vacuum**: hand pages the engine has *already freed* back to the filesystem, 256 at a
  time. Deleting rows never shrinks a SQLite file on its own — freed pages are recycled internally —
  so reclamation is a separate, deliberate act, triggered by evidence from the accounting report
  (reclaimable share crossing 25% of a file of at least 16 MiB), not by a schedule.

`Store::maintenance_pass` has no pruning parameter. There is no code path in this product that can
delete a user's history, which is what makes the unbounded-retention decision safe to leave standing.

Databases created from 2026-08-24 are `auto_vacuum=INCREMENTAL`, which is fixed at creation and is
what makes chunked reclamation possible at all. **An older file cannot reclaim incrementally**; its
report says so in `reclaim_note`, names the offline remedy (stop the API, `VACUUM;`, optionally
`PRAGMA auto_vacuum=INCREMENTAL; VACUUM;`) and states what that remedy costs in free disk — roughly
twice the file size, worth checking before starting on a nearly-full volume.

### The window is found, not scheduled

Every pass is gated on an **activity gauge**: a live count of in-flight requests, incremented and
decremented at the router's front door, over *all* routes (a long analytical read holds a WAL
snapshot and is exactly the foreground work a checkpoint must not compete with). The gate is two
conditions — the gauge reads zero **and** a minimum interval has elapsed. The interval bounds cost;
the gauge bounds interference; either one alone is a known failure (a wall-clock timer that fires
mid-request, or a busy loop that maintains in every momentary gap).

Deferral has a ladder, so politeness cannot decay into no maintenance at all:

| rung | opens when | chunk |
| --- | --- | --- |
| quiet | gauge is 0 and the minimum interval elapsed | full |
| quieter | no quiet window past the staleness bound, gauge ≤ 1 | reduced |
| escalated | the journal is over its hard bound, or ≥25% of the file is reclaimable | full, and it does not yield |

The escalation bounds are stated as **harms in bytes**, never as elapsed time: "the journal exceeds
64 MiB" is a reason a human can weigh; "it has been a week" is the wall clock sneaking back in.

Long passes run as chunks with the gauge re-read between them, and the store's write lock is released
before each re-read — the reverse order would leave a user waiting on the very check meant to protect
them. A pass abandoned at a chunk boundary is merely incomplete, never inconsistent.

**What the escalated rung is actually for, measured.** The store soak lane
(`docs/harness/soak-lane.md`) ran two CI certifications on 2026-08-24 and found that under
*continuous* read load the journal plateaus at ~57–59 MiB rather than growing without bound — 90 s and
300 s runs agreed within 3% despite the second doing 2.8× the work. The cause is that a checkpoint
cannot advance past a live reader's snapshot, so with readers looping without a gap the checkpointer
starves. On such an instance the activity gauge never reads zero, the sweep defers every time, and
the journal's own byte bound is the only thing that ever runs a pass. That is the rung working as
designed — and it is why the bound is stated in bytes: 64 MiB sits just above the measured plateau,
so an instance in this state checkpoints occasionally rather than never, without a wall clock
deciding it.

`LIGHTTRACK_MAINTENANCE_SECS=0` switches the sweep off entirely. It is **on by default**, unlike the
forecast sweep (§10a): that one turns a self-hosted process into an outbound notifier, which is a
decision; this one keeps the process's own disk in order, which is upkeep.

### Deferral is an outcome

`ran`, `nothing_to_do`, `deferred` and `failed` are four different results and are counted
separately. A log that recorded only successes could not tell a healthy store from a scheduler that
had been deferring for a month — and the discovery would arrive as a disk-full report. `last_run` is
`null` until a pass has actually run, because "never ran" is its own state, not a zero.
