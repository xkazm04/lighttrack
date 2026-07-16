# Feature Scout — Platform Core & API Server

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. `/health` is a static string — no readiness check against the store
- **Severity**: High
- **Category**: operability
- **File**: `crates/api/src/main.rs:323-325` (`async fn health() -> "ok"`); router wire `main.rs:232`
- **Scenario**: An operator puts LightTrack behind a k8s/Docker/Nginx health probe pointed at `GET /health`. The Postgres/SQLite backend goes away (connection pool exhausted, file lock, Postgres restart), but `/health` keeps returning `200 "ok"` because the handler never touches `state.store`. The load balancer keeps routing ingest traffic to a server that 500s on every `POST /v1/events` — silent data loss of exactly the telemetry this product exists to capture.
- **Root cause**: `health()` takes no `State`, does no work, and returns a hardcoded `&'static str`. There is no readiness probe that exercises the store, and no way to distinguish "process is up" (liveness) from "process can serve requests" (readiness). The startup banner (`main.rs:214`) knows `backend`, version, price count, and auth mode, but none of that is queryable at runtime.
- **Impact**: Probes give false-green during a dependency outage; operators lose the single most basic signal a self-hosted backend must provide. This is table-stakes for anything deployed to a container orchestrator.
- **Fix sketch**: Add `GET /health/ready` (or extend `/health` to accept `?deep=1`) that runs a cheap store round-trip via `spawn_db` (e.g. `store.list_prices()` count or a dedicated `store.ping()`), returning `{"status":"ok","backend":"postgres","version":"x.y.z","prices":N}` on success and `503` (a new `ErrorCode`/status, or reuse `Internal`→500) when the round-trip fails. Keep the bare `/health` as a zero-dependency liveness probe. Thread `State<AppState>` into the handler.
- **Trade-offs**: A deep probe adds one DB query per poll; document the split (liveness = `/health`, readiness = `/health/ready`) so operators wire the right one.

## 2. The observability backend exposes no self-metrics endpoint
- **Severity**: High
- **Category**: observability
- **File**: `crates/api/src/main.rs:228-317` (`build_router`) — no `/metrics` route; `main.rs:189-212` (state assembled without a metrics registry)
- **Scenario**: A team self-hosts LightTrack to watch their LLM spend. To trust it, they need to watch LightTrack itself: ingest requests/sec, 4xx/5xx rates, ingest rejection (429) rate, batch queue latency, store call duration, in-flight relay tasks. Today there is nothing to scrape — no Prometheus `/metrics`, no OpenMetrics, no counters. The product measures everyone else's LLM calls but cannot be measured. The only runtime signal is `println!`/`eprintln!` to stdout (`main.rs:169-219`).
- **Root cause**: `build_router` wires 60+ domain routes but no metrics surface, and `AppState` carries no metrics recorder. The in-memory `RejectionLedger` (`state.rs:46`) already tracks 429'd ingest counts for the `/v1/limits/status` endpoint — proof the server wants operational counters but has nowhere standard to publish them. There is no `tower_http` metrics/trace layer on the router either.
- **Impact**: Operators can't alert on LightTrack's own error budget or throughput, can't capacity-plan, and can't tell "ingest dropped to zero" from "traffic dropped to zero". For a monitoring product this is a credibility gap as much as a feature gap.
- **Fix sketch**: Add a `metrics`/`metrics-exporter-prometheus` recorder (or hand-rolled `prometheus` registry) into `AppState`; wrap the router in a `tower` middleware that increments request-count/duration/status histograms keyed by route+method; expose `GET /metrics` (admin-gated via `ensure_can_admin`, or bind-restricted) rendering the text exposition format. Seed with the counters already implied: ingest accepted/rejected (fold in `RejectionLedger`), store-call latency (wrap `spawn_db`), relay lease depth.
- **Trade-offs**: Adds a dependency and a small per-request recording cost; gate or bind `/metrics` to avoid leaking route cardinality publicly.

## 3. No graceful shutdown — SIGTERM drops in-flight requests and detached writes
- **Severity**: Medium
- **Category**: operability
- **File**: `crates/api/src/main.rs:223-225` (`axum::serve(listener, app).await?` with no `.with_graceful_shutdown(...)`); detached tasks at `crates/api/src/guards.rs:47-51`
- **Scenario**: `docker stop` / a k8s rolling deploy sends SIGTERM. Tokio's default behavior aborts the runtime immediately: in-flight `POST /v1/events/batch` requests are cut mid-write, and the fire-and-forget tasks spawned during auth (`touch_api_key`, `guards.rs:47`) and alert delivery are dropped without completing. During every deploy the operator gets a burst of client-side connection resets and lost last-use/alert writes.
- **Root cause**: `main` calls `axum::serve(listener, app).await?` with no shutdown signal wired, and there is no signal handler (`tokio::signal::ctrl_c` / unix SIGTERM) anywhere in `main.rs`. Nothing drains in-flight work or the detached `tokio::spawn` side-writes before exit.
- **Impact**: Every deploy/restart causes avoidable request failures and small silent data loss; also makes the server unfriendly to orchestrators that expect a clean drain within `terminationGracePeriod`.
- **Fix sketch**: Add an async `shutdown_signal()` awaiting `ctrl_c()` plus (unix) SIGTERM, pass it to `axum::serve(...).with_graceful_shutdown(shutdown_signal())`, and log "draining" so operators see the transition. Optionally track/`JoinSet` the detached auth/alert spawns so they can be awaited briefly on shutdown.
- **Trade-offs**: None material; behind a bounded drain timeout it only improves restart hygiene.
