# Performance Optimizer — Billing Integrations (Stripe/Polar)

> Total: 2
> Critical: 0 | High: 1 | Medium: 1 | Low: 0

## 1. Backfill sync POSTs one HTTP request per invoice (sequential, no batching)
- **Severity**: High
- **Category**: sync-external-call
- **File**: `crates/runner/src/billing.rs:69-75`
- **Scenario**: `lt-runner billing sync stripe --project X --days 90` on an account with real volume. Stripe is paged 100 invoices at a time; each normalized invoice triggers its own `post(cli, http, "/v1/revenue", …)`. A 90-day reconcile of a busy project = thousands of invoices → thousands of *serial* HTTP round-trips to the LightTrack API, each of which in turn takes the API's global `Mutex<Connection>` for a single-row write.
- **Root cause**: The inner `for inv in &data` loop does one blocking `post()` per record. The blocking `reqwest` client blocks the thread on each request/response before the next begins, so total wall time ≈ N × (RTT + API DB-write time). There is no bulk endpoint use and no pipelining, even though the events for a page are already materialized in memory.
- **Impact**: For N=5,000 invoices at ~15 ms per localhost round-trip the pull spends ~75 s purely in serialized POST latency; against a remote API (20–50 ms RTT) it is minutes, and it holds the API's global SQLite write lock for one tiny transaction N times instead of once. Wall time scales linearly with invoice count with a large per-item constant.
- **Fix sketch**: POST a page's events as one batch (a `/v1/revenue` array body reusing the same all-or-nothing `insert_revenue_events` path the webhook uses), so it's one request + one DB transaction per 100 invoices — a ~100× cut in round-trips and lock acquisitions. If the endpoint must stay single-record, at minimum bound-concurrency the per-page POSTs (e.g. a small futures/threadpool fan-out) to overlap RTTs.
- **Trade-offs**: Batching weakens per-record idempotency granularity slightly (a batch fails whole), but the store upsert is already deterministic-id idempotent, so a retried batch is safe. Backfill is an occasional operator action, which caps the blast radius — hence High, not Critical.

## 2. Webhook handler serializes two global-mutex DB round-trips per delivery
- **Severity**: Medium
- **Category**: hot-path
- **File**: `crates/api/src/billing.rs:50-82`
- **Scenario**: Live Stripe/Polar webhooks fire per paid order / renewal / refund. Each delivery does two sequential `spawn_db` hops — first `get_project(&project_id)` to validate the `?project=`, then `insert_revenue_events`. Under the SQLite store's single global `Mutex<Connection>`, both hops contend the one connection lock, so a webhook burst (e.g. a subscription renewal wave) serializes two lock acquisitions per event plus the thread-hop overhead of two `spawn_db` calls.
- **Root cause**: The project-existence check is a full extra query issued on *every* delivery even though the project set is tiny and changes rarely; it can't overlap the insert because it must complete first, doubling the serialized DB section and the blocking-pool round-trips per webhook.
- **Impact**: ~2× the DB-lock hold count and blocking-task dispatches per webhook versus one combined hop. Bounded (webhook QPS is modest), but under the global-mutex store it directly widens the serialized critical section during renewal bursts.
- **Fix sketch**: Cache known-valid project ids in memory (the same map already held in `AppState`), or fold the existence check into the insert transaction (validate + insert in one `spawn_db` closure / one lock acquisition) so the common path is a single round-trip. A short-TTL project-id set removes the per-delivery validation query entirely for steady traffic.
- **Trade-offs**: A cached project set can briefly lag a just-created project; keep the DB check as a fallback on cache-miss so correctness (reject phantom projects before marking seen) is preserved.

---

**Checked and deliberately not filed:**
- `shared_fx()` called per pagination page in the runner loop — it returns an `Arc` clone from a shared table; negligible, micro-opt.
- `resp.get("data")...cloned()` deep-clones a page's ≤100 JSON values in the runner — a real avoidable allocation, but bounded to a rare backfill and small vs. the network cost in finding #1; micro-optimization, dropped.
- `decode_hex` / per-verify HMAC allocation in `stripe.rs` and the base64/`ct_eq` path in `polar.rs` — one small Vec per webhook, not a hot-path cost.
