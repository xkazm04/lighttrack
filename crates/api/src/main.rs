//! LightTrack API — ingest + query + project/key/limit management, scoring, benchmarks, jobs.
//!
//! This file is wiring only: build the store + price book, build the router, serve. Handlers live
//! in per-domain modules (`events`, `scores`, `prices`, `datasets`, `rubrics`, `benchmarks`,
//! `jobs`, `projects`, `limits`); shared plumbing in `state`, `error`, `guards`, `auth`.
//!
//! Routes:
//!   GET  /health                         liveness + the store backend's declared surfaces
//!   GET  /v1/capabilities                what this deployment's store backend serves, and what
//!                                        it answers 501 for (any authenticated principal)
//!   POST /v1/events                      ingest one event (cost computed; limits evaluated)
//!   GET  /v1/ingest/status               load-shedding view: in-flight depth + shed/timeout counts
//!   GET  /v1/storage/status              (admin) disk accounting per table + index, the store's own
//!                                        per-family latency, and the maintenance flight recorder —
//!                                        including the passes that were DEFERRED. Retention is
//!                                        deliberately unbounded (operator 2026-08-24); this is where
//!                                        that growth is visible. See docs/ARCHITECTURE.md §12.
//!   POST /v1/events/batch                ingest an array; per-item accepted|rejected|invalid (HTTP 200)
//!   GET  /v1/events?project=&limit=&since=&until=&provider=&model=&trace_id=&name=
//!                  &status=&tag=&meta=&min_cost=&count=&cursor=
//!                                        keyset pagination: next page cursor in `X-Next-Cursor`;
//!                                        `count=1` also returns `X-Total-Count` (whole match set).
//!                                        `meta` is `key` or `key=value` (metadata predicate).
//!                                        Backends without the extended predicates answer 501
//!                                        `unsupported` rather than silently ignoring a filter.
//!   GET  /v1/events/:id
//!   POST /v1/traces                      OTLP/HTTP JSON export: OTel GenAI spans -> events (see `otlp`)
//!   GET  /v1/traces?project=&limit=     list traces (rollups grouped by trace_id)
//!   GET  /v1/traces/:id                 one trace: totals + span tree + scores within it
//!   POST /v1/traces/:id/score           score a whole trace (anchored to its root span)
//!   GET  /v1/costs?project=&since=&until=
//!   GET  /v1/usecases?project=&since=   use-case rollup: usage+cost by name×provider×model, windowed
//!   POST /v1/scores  GET /v1/scores?project=&limit=[&run=]   (`run` = one benchmark run's cases)
//!   GET  /v1/prices  PUT /v1/prices/:provider/:model
//!   .../datasets .../rubrics .../benchmarks .../jobs            (see modules)
//!   GET  /v1/benchmarks/:id/gate         CI-gate verdict from the latest finished run
//!                                        (pass|regressed|no_baseline|no_runs + run_id/mean/baseline/n)
//!   POST /v1/projects/:id/prompts  GET /v1/projects/:id/prompts          prompt registry
//!   GET  /v1/projects/:id/prompts/:name?label=production|version=N       runtime fetch by label
//!   POST /v1/projects/:id/prompts/:name/versions                         new version (auto-benchmarks)
//!   POST /v1/projects/:id/prompts/:name/promote                          label promote (regression-gated)
//!   POST /v1/projects  GET /v1/projects   POST /v1/projects/:id/keys
//!   PUT  /v1/projects/:id                update name/enabled/redaction/collective_opt_in (admin);
//!                                        a redaction change is enforced on the NEXT ingested event
//!   POST /v1/projects/:id/limits  GET /v1/projects/:id/limits
//!   PUT  /v1/limits/:id  DELETE /v1/limits/:id   update (incl. enable/disable) or remove a rule
//!   GET  /v1/limits/status?project=      evaluate limits -> throttle flag + per-rule status, plus a
//!                                        `rejected` block (count + est_missed_cost_usd + window) of
//!                                        429'd ingest attempts per breached rule. That ledger is
//!                                        best-effort and process-local: it lives in memory, resets on
//!                                        restart, and rolls entries off after 24h (rejected events are
//!                                        never stored — that would corrupt the usage/cost rollups).
//!   GET  /v1/limits/usage?project=&by=&window=&limit=
//!                                        rolling usage grouped by ONE scope dimension
//!                                        (api_key | customer | model | provider | name), each row
//!                                        carrying the scoped rules that bind it. Answers "which key
//!                                        is spending" BEFORE a cap trips, and "which key drove this
//!                                        breach" after — over the API, not only via an alert channel.
//!                                        501 `unsupported` on backends without the grouped query.
//!   POST /v1/relay/tasks                 enqueue a device task (GET ?project=&status=&limit= lists)
//!   GET  /v1/relay/tasks/:id             task status/result (the originating app polls this)
//!   POST /v1/relay/lease                 device: lease due tasks (device key; outbound-only)
//!   POST /v1/relay/tasks/:id/result      device: report succeeded | failed | deferred
//!   POST /v1/revenue                     record revenue (manual / billing sync) for profit tracking
//!   GET  /v1/margin?by=customer|product&since=&until=&below=<pct>   revenue − LLM cost rollup
//!   GET  /v1/margin/trend?by=&days=&top=   per-day revenue/cost/margin series per customer/product
//!   GET  /v1/margin/customer/:id?since=&until=   one customer's revenue+cost by model & use-case
//!   GET  /v1/margin/simulate?by=&price_per_mtok=&flat_monthly=&since=&until=   pricing what-if (read-only)
//!   GET  /v1/forecast?project=&by=&horizon=&lookback=   projected spend/budget-breach + margin-erosion + pre-emptive alerts
//!        The same alerts also fire on a schedule with no request involved when
//!        LIGHTTRACK_FORECAST_SWEEP_SECS is set (off by default; see `forecast_sweep`).
//!   POST /v1/billing/:provider/webhook?project=   signed Stripe/Polar webhook → revenue (unauth; HMAC)
//!   GET  /v1/collective/digest?min_cases=     build this instance's privacy-safe model digest (admin)
//!   POST /v1/collective/ingest                hub: accept a contributor's digest (gated; off default)
//!   GET  /v1/collective/leaderboard?task_type=&provider=&judge=   merged real-world model leaderboard
//!   DEL  /v1/collective/contribution        withdraw this source's contributed entries
//!
//! Env: LIGHTTRACK_BIND, LIGHTTRACK_DB, LIGHTTRACK_DATABASE_URL, LIGHTTRACK_PRICING,
//!      LIGHTTRACK_MAX_TS_SKEW_SECS (symmetric client-`ts` skew bound in seconds; 0 = disable the
//!        check entirely; unset = the asymmetric defaults below),
//!      LIGHTTRACK_MAX_TS_SKEW_FUTURE_SECS (max seconds `ts` may lead server time; default 300),
//!      LIGHTTRACK_MAX_TS_SKEW_PAST_SECS (max seconds `ts` may lag server time; default 7 days),
//!      LIGHTTRACK_REDACTION_CACHE_TTL_SECS (staleness bound on the per-project redaction-policy
//!        cache; default 60, 0 = never cache),
//!      LIGHTTRACK_MAX_BODY_BYTES (single-event ingest body cap → 413; default 2 MiB),
//!      LIGHTTRACK_MAX_BATCH (max items per POST /v1/events/batch; default 500),
//!      LIGHTTRACK_MAX_BATCH_BODY_BYTES (batch ingest body cap → 413; default 8 MiB),
//!      LIGHTTRACK_INGEST_MAX_INFLIGHT (bounded in-flight ingest; over it → 503 `overloaded` +
//!        Retry-After, distinct from the 429 `rate_limited` that means "over budget"; default 64,
//!        0 = unbounded),
//!      LIGHTTRACK_INGEST_TIMEOUT_SECS (ingest deadline → 504 `timeout`; default 10, 0 = off),
//!      LIGHTTRACK_INGEST_RETRY_AFTER_SECS (Retry-After advertised when shedding; default 1),
//!      LIGHTTRACK_AUTH_MODE (dev|enforced), LIGHTTRACK_ADMIN_KEY,
//!      LIGHTTRACK_AUTH_MAX_FAILURES (failed credential attempts one source may make per window
//!        before it is refused with 429 `rate_limited` + Retry-After; default 10, 0 = off),
//!      LIGHTTRACK_AUTH_FAILURE_WINDOW_SECS (that window; default 60),
//!      LIGHTTRACK_AUTH_THROTTLE_MAX_SOURCES (bound on tracked sources; default 4096),
//!      LIGHTTRACK_AUTH_TRUSTED_PROXY_HOPS (trust X-Forwarded-For from this many proxies in front of
//!        the instance; default 0 = never — an untrusted XFF both evades and poisons the throttle),
//!      LIGHTTRACK_RELAY_DEVICE_KEY (bearer key of the enrolled local device — relay lease/result),
//!      LIGHTTRACK_RELAY_FLAT_COST_USD (fixed cost stamped per relay run event; default 1.0),
//!      LIGHTTRACK_ALERT_WEBHOOK / LIGHTTRACK_ALERT_NTFY / LIGHTTRACK_ALERT_COOLDOWN_SECS (see alerts),
//!      LIGHTTRACK_FORECAST_SWEEP_SECS (cadence of the scheduled budget-ETA / margin-erosion alert
//!        sweep; unset or 0 = off, floor 60s), LIGHTTRACK_FORECAST_SWEEP_HORIZON /
//!        LIGHTTRACK_FORECAST_SWEEP_LOOKBACK (projection shape; default 14/14 days),
//!      LIGHTTRACK_MAINTENANCE_SECS (how often the quiet-window maintenance gate is evaluated;
//!        default 300, floor 30, 0 = no maintenance at all — the journal and the freelist then grow
//!        unattended), LIGHTTRACK_MAINTENANCE_MIN_INTERVAL_SECS (minimum spacing between passes;
//!        default 900), LIGHTTRACK_MAINTENANCE_STALE_SECS (how long deferral may continue before a
//!        reduced-chunk pass is accepted against light traffic; default 3600),
//!        LIGHTTRACK_MAINTENANCE_WAL_HARD_BYTES (journal size that is itself the harm, past which a
//!        pass runs regardless of activity; default 64 MiB),
//!      LIGHTTRACK_BENCH_WEBHOOK (benchmark-run completion webhook; falls back to LIGHTTRACK_ALERT_WEBHOOK),
//!      LIGHTTRACK_LOG (level or full tracing filter directive; default `info`, falls back to RUST_LOG),
//!      LIGHTTRACK_LOG_FORMAT (json — the default, one indexed JSON object per line on stdout — | text),
//!      LIGHTTRACK_REDACT_INGEST (unset/all = scrub PII from every project — the DEFAULT since D14 —
//!        | off = store client text verbatim | csv of project_ids = scrub only those; see redact),
//!      LIGHTTRACK_COLLECTIVE_ID (opaque source id — hashed before contribution),
//!      LIGHTTRACK_COLLECTIVE_ACCEPT (1|true — this instance is a leaderboard hub; off by default),
//!      LIGHTTRACK_COLLECTIVE_ALLOW_ANON (1|true — hub accepts keyless pushes under one shared
//!        `anonymous` identity; off by default, a keyless push is otherwise refused),
//!      LIGHTTRACK_COLLECTIVE_MIN_CASES (hub-enforced k-anonymity floor; default 5, clamp ≥1),
//!      LIGHTTRACK_COLLECTIVE_DISPLAY_FLOOR (merged rows below this many cases are flagged
//!        low_confidence; default 30),
//!      LIGHTTRACK_MODEL_ALIASES (model-identity normalization table; default config/model_aliases.json).

mod alerts;
mod auth;
mod auth_scopes;
mod auth_throttle;
mod benchmarks;
mod billing;
mod capabilities;
mod collective;
mod datasets;
mod error;
mod events;
mod events_admission;
mod events_batch;
mod events_query;
mod events_validate;
mod forecast;
mod forecast_alerts;
mod forecast_sweep;
mod guards;
mod idempotency;
mod jobs;
mod limits;
mod limits_usage;
mod logging;
mod otlp;
mod prices;
mod projects;
mod projects_keys;
mod prompts;
mod redact;
mod rejections;
mod relay;
mod revenue;
mod rubrics;
mod scores;
mod shed;
mod state;
mod storage;
mod traces;

#[cfg(test)]
mod tests_auth_throttle;
#[cfg(test)]
mod tests_capabilities;
#[cfg(test)]
mod tests_collective;
#[cfg(test)]
mod tests_dev_mode;
#[cfg(test)]
mod tests_forecast;
#[cfg(test)]
mod tests_ingest;
#[cfg(test)]
mod tests_limit_scope;
#[cfg(test)]
mod tests_relay;
#[cfg(test)]
mod tests_storage;
#[cfg(test)]
mod tests_tenancy;
#[cfg(test)]
mod tests_traces;

use std::sync::{Arc, RwLock};

use axum::{
    extract::DefaultBodyLimit,
    routing::{delete, get, post, put},
    Router,
};

use lighttrack_core::PriceBook;
use lighttrack_store::{SqliteStore, Store};

use auth::AuthMode;
use state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // First, before anything can want to say something: every diagnostic below this line is a
    // structured event on stdout (see `logging`).
    logging::init();

    let bind = env_or("LIGHTTRACK_BIND", "127.0.0.1:8787");
    let db = env_or("LIGHTTRACK_DB", "data/lighttrack.db");
    let pricing = env_or("LIGHTTRACK_PRICING", "config/pricing.json");
    let auth_mode = AuthMode::from_env(&env_or("LIGHTTRACK_AUTH_MODE", "dev"));
    let admin_key = std::env::var("LIGHTTRACK_ADMIN_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    let relay_device_key = std::env::var("LIGHTTRACK_RELAY_DEVICE_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    let relay_flat_cost = std::env::var("LIGHTTRACK_RELAY_FLAT_COST_USD")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0);

    // Backend selection: LIGHTTRACK_DATABASE_URL=postgres://... → Postgres; else SQLite at LIGHTTRACK_DB.
    let database_url = std::env::var("LIGHTTRACK_DATABASE_URL")
        .ok()
        .filter(|s| !s.is_empty());
    let backend = match database_url.as_deref() {
        Some(u) if u.starts_with("postgres") => "postgres",
        Some(u) if u.starts_with("firestore") => "firestore",
        _ => "sqlite",
    };

    // The Postgres store calls `block_on` internally, which panics if run on the async main thread.
    // Do the connect + seeding on a blocking thread; the request handlers already use spawn_blocking.
    type StartupState = (
        Arc<dyn Store + Send + Sync>,
        PriceBook,
        std::collections::HashMap<String, state::ProjectPolicy>,
    );
    let (store, book, project_policies) = tokio::task::spawn_blocking(
        move || -> anyhow::Result<StartupState> {
            let store: Arc<dyn Store + Send + Sync> = match &database_url {
                Some(url) if url.starts_with("postgres") => {
                    Arc::new(lighttrack_store_pg::PgStore::connect(url)?)
                }
                Some(url) if url.starts_with("firestore") => {
                    Arc::new(lighttrack_store_firestore::FirestoreStore::connect(url)?)
                }
                _ => Arc::new(SqliteStore::open(&db)?),
            };

            // Seed the price book from pricing.json on first run; thereafter the DB is the source of truth.
            if store.list_prices()?.is_empty() {
                let (seed, from) = crate::prices::seed_book(&pricing);
                for row in seed.rows() {
                    store.upsert_price(&row)?;
                }
                let source = match from {
                    crate::prices::PriceSeed::File => pricing.clone(),
                    crate::prices::PriceSeed::Embedded => "compiled-in default".to_string(),
                };
                tracing::info!(count = seed.len(), source = %source, "seeded model prices into the DB");
            }
            let book = PriceBook::from_rows(&store.list_prices()?);
            // Warm the per-project persistence-policy cache here too: this closure is the one
            // startup context allowed to call the store synchronously (Postgres `block_on`s
            // internally and panics on the async main thread — created-after-startup projects
            // are added on create / first sight).
            let project_policies: std::collections::HashMap<_, _> = store
                .list_projects()
                .unwrap_or_default()
                .into_iter()
                .map(|p| (p.id.clone(), state::ProjectPolicy::from(&p)))
                .collect();
            Ok((store, book, project_policies))
        },
    )
    .await??;
    let n_prices = book.len();

    let alerts = Arc::new(alerts::Alerter::from_env());
    let alerts_desc = alerts.describe();
    let redact = Arc::new(redact::Redactor::from_env());
    let redact_desc = redact.describe();
    let billing = Arc::new(lighttrack_billing::BillingRegistry::from_env());
    let billing_desc = billing.describe();
    let collective = Arc::new(collective::Collective::from_env());
    let collective_desc = collective.describe();
    collective.warn_if_hub_is_weak(auth_mode);
    let seen_webhooks = Arc::new(idempotency::SeenWebhooks::new(
        idempotency::DEFAULT_CAPACITY,
    ));
    let rejections = Arc::new(rejections::RejectionLedger::new());
    let ingest_guard = Arc::new(shed::IngestGuard::from_env());
    let shed_desc = ingest_guard.describe();
    let auth_throttle = Arc::new(auth_throttle::AuthThrottle::from_env());
    let auth_throttle_desc = auth_throttle.describe();
    let maintenance_cfg = storage::SweepConfig::from_env();
    let maintenance_desc = storage::describe(maintenance_cfg);
    let state = AppState {
        store,
        prices: Arc::new(RwLock::new(book)),
        auth_mode,
        admin_key,
        relay_device_key,
        relay_flat_cost,
        alerts,
        redact,
        billing,
        collective,
        seen_webhooks,
        rejections,
        ingest_guard,
        auth_throttle,
        project_policies: Arc::new(state::ProjectPolicyCache::new(project_policies)),
        activity: Arc::new(storage::ActivityGauge::default()),
        maintenance: Arc::new(storage::Maintenance::default()),
        maintenance_desc: maintenance_desc.clone(),
    };

    let sweep = forecast_sweep::SweepConfig::from_env();
    let sweep_desc = forecast_sweep::describe(sweep);
    // The whole runtime configuration as one indexed event: "why did prod behave differently" is a
    // field comparison across two boots, not a diff of two prose lines. The human-critical part (are
    // we up, and on what address) stays in the message so it reads at a glance in either format.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        store = backend,
        priced_models = n_prices,
        auth = ?state.auth_mode,
        admin_key = if state.admin_key.is_some() { "set" } else { "unset" },
        auth_throttle = %auth_throttle_desc,
        alerts = %alerts_desc,
        forecast_sweep = %sweep_desc,
        ingest = %shed_desc,
        maintenance = %maintenance_desc,
        redact = %redact_desc,
        billing = %billing_desc,
        collective = %collective_desc,
        "lighttrack-api v{} listening on http://{bind}",
        env!("CARGO_PKG_VERSION"),
    );
    // Redaction is a *storage* posture: what an operator believes is in the DB has to match what is
    // actually in it, and the default changed (D14). Its own line, at a level that matches the risk.
    state.redact.log_posture();
    // What this backend can and cannot serve, named once at boot. Until the manifest existed the
    // only record of a gap was a 501 someone hit in production.
    capabilities::log_posture(&state.store.capabilities());
    // `auth=Dev` in the banner above is one field among many; an unauthenticated server deserves a
    // block you cannot skim past, so that one stays a raw multi-line stderr shout rather than
    // becoming a JSON string with `\n`s in it.
    auth::warn_if_unenforced(state.auth_mode);

    // Pre-emptive forecast alerts on a timer (off unless configured). Detached: it never shares a
    // task with a request, and its store reads go to the blocking pool like any handler's.
    forecast_sweep::spawn(state.clone(), sweep);

    // Quiet-window store maintenance: checkpoint the journal and hand already-freed pages back to
    // the filesystem, gated on the activity gauge. Lossless — it never deletes a row — and every
    // pass, including every deferral, lands in the flight recorder behind /v1/storage/status.
    storage::spawn(state.clone(), maintenance_cfg);

    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    // `into_make_service_with_connect_info` is what puts the socket peer address in each request's
    // extensions. Without it there is no source identity and `auth_throttle` silently does nothing —
    // so this is not an optional nicety, it is the throttle's input.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

pub(crate) fn build_router(state: AppState) -> Router {
    let body_limit = events_validate::body_limit_bytes();
    let batch_body_limit = events_validate::batch_body_limit_bytes();
    // Load shedding is layered onto the ingest POST *methods* only: a bounded write path is what
    // keeps the server responsive under overload, while the operator's own reads (including
    // `/v1/ingest/status`, the surface that says whether we ARE shedding) stay answerable.
    let shed_ingest = axum::middleware::from_fn_with_state(state.clone(), shed::ingest_admission);
    Router::new()
        .route("/health", get(capabilities::health))
        .route("/v1/capabilities", get(capabilities::get_capabilities))
        .route(
            "/v1/events",
            post(events::post_event)
                .layer(shed_ingest.clone())
                .get(events_query::get_events)
                .layer(DefaultBodyLimit::max(body_limit)),
        )
        .route(
            "/v1/events/batch",
            post(events_batch::post_batch)
                .layer(DefaultBodyLimit::max(batch_body_limit))
                .layer(shed_ingest.clone()),
        )
        .route("/v1/ingest/status", get(shed::get_ingest_status))
        .route("/v1/storage/status", get(storage::get_storage_status))
        .route("/v1/events/:id", get(events_query::get_event_by_id))
        .route(
            "/v1/traces",
            // The OTLP door is an ingest door: one export fans a whole batch into the same write
            // path, so it takes a shed permit exactly like `/v1/events/batch`. Same ordering trick
            // as the routes above — the layer wraps only the methods added before it, so the GET
            // added afterwards stays unguarded: an operator's reads must answer while we shed.
            post(otlp::post_traces)
                .layer(shed_ingest)
                .get(traces::list_traces)
                .layer(DefaultBodyLimit::max(batch_body_limit)),
        )
        .route("/v1/traces/:id", get(traces::get_trace))
        .route("/v1/traces/:id/score", post(traces::score_trace))
        .route("/v1/costs", get(events_query::get_costs))
        .route("/v1/costs/prompts", get(events_query::get_prompt_costs))
        .route("/v1/usecases", get(events_query::get_usecases))
        .route(
            "/v1/scores",
            post(scores::post_score).get(scores::get_scores),
        )
        .route("/v1/prices", get(prices::get_prices))
        .route("/v1/prices/:provider/:model", put(prices::put_price))
        .route(
            "/v1/projects/:id/datasets",
            post(datasets::create_dataset).get(datasets::list_datasets),
        )
        .route("/v1/datasets/:id", get(datasets::get_dataset))
        .route(
            "/v1/datasets/:id/items",
            post(datasets::add_dataset_item).get(datasets::list_dataset_items),
        )
        .route("/v1/datasets/:id/freeze", post(datasets::freeze_dataset))
        .route(
            "/v1/projects/:id/rubrics",
            post(rubrics::create_rubric).get(rubrics::list_rubrics),
        )
        .route("/v1/rubrics/:id", get(rubrics::get_rubric))
        .route(
            "/v1/projects/:id/benchmarks",
            post(benchmarks::create_benchmark).get(benchmarks::list_benchmarks),
        )
        .route("/v1/benchmarks/:id", get(benchmarks::get_benchmark))
        .route(
            "/v1/benchmarks/:id/runs",
            get(benchmarks::list_benchmark_runs),
        )
        .route("/v1/benchmarks/:id/gate", get(benchmarks::benchmark_gate))
        .route("/v1/benchmark-runs", post(benchmarks::post_benchmark_run))
        .route("/v1/benchmarks/:id/enqueue", post(jobs::enqueue_benchmark))
        .route(
            "/v1/projects/:id/prompts",
            post(prompts::create_prompt).get(prompts::list_prompts),
        )
        .route("/v1/projects/:id/prompts/:name", get(prompts::get_prompt))
        .route(
            "/v1/projects/:id/prompts/:name/versions",
            post(prompts::add_version).get(prompts::list_versions),
        )
        .route(
            "/v1/projects/:id/prompts/:name/promote",
            post(prompts::promote),
        )
        .route("/v1/jobs", get(jobs::list_jobs))
        .route("/v1/jobs/claim", post(jobs::claim_job))
        .route("/v1/jobs/:id", get(jobs::get_job))
        .route("/v1/jobs/:id/cancel", post(jobs::cancel_job))
        .route("/v1/jobs/:id/progress", post(jobs::job_progress))
        .route("/v1/jobs/:id/renew", post(jobs::job_renew))
        .route("/v1/jobs/:id/finish", post(jobs::job_finish))
        .route(
            "/v1/projects",
            post(projects::create_project).get(projects::list_projects),
        )
        .route(
            "/v1/projects/:id",
            put(projects::update_project).delete(projects::archive_project),
        )
        .route(
            "/v1/projects/:id/keys",
            post(projects_keys::create_key).get(projects_keys::list_keys),
        )
        .route(
            "/v1/projects/:id/keys/:kid",
            delete(projects_keys::revoke_key),
        )
        .route(
            "/v1/projects/:id/keys/:kid/rotate",
            post(projects_keys::rotate_key),
        )
        .route(
            "/v1/projects/:id/limits",
            post(limits::create_limit).get(limits::list_limits),
        )
        .route(
            "/v1/limits/:id",
            put(limits::update_limit).delete(limits::delete_limit),
        )
        .route("/v1/limits/status", get(limits::limits_status))
        .route("/v1/limits/usage", get(limits_usage::usage_by_scope))
        .route(
            "/v1/relay/tasks",
            post(relay::enqueue_task).get(relay::list_tasks),
        )
        .route("/v1/relay/tasks/:id", get(relay::get_task))
        .route("/v1/relay/tasks/:id/result", post(relay::post_result))
        .route("/v1/relay/lease", post(relay::lease_tasks))
        .route("/v1/revenue", post(revenue::post_revenue))
        .route("/v1/margin", get(revenue::get_margin))
        .route("/v1/margin/trend", get(revenue::get_margin_trend))
        .route("/v1/margin/customer/:id", get(revenue::get_customer_margin))
        .route("/v1/margin/simulate", get(revenue::get_margin_simulate))
        .route("/v1/forecast", get(forecast::get_forecast))
        .route("/v1/billing/:provider/webhook", post(billing::post_webhook))
        .route("/v1/collective/digest", get(collective::get_digest))
        .route("/v1/collective/ingest", post(collective::post_ingest))
        .route(
            "/v1/collective/leaderboard",
            get(collective::get_leaderboard),
        )
        .route(
            "/v1/collective/contribution",
            delete(collective::delete_contribution),
        )
        // Over every route: the maintenance sweep's activity gauge. It must see ALL foreground work,
        // not just the ingest doors — a long analytical read holds a WAL snapshot and is exactly the
        // work a checkpoint should not compete with — so it is layered here rather than beside the
        // ingest shed. The token decrements on drop, so a panicking or cancelled handler cannot
        // leave the gauge permanently busy and silently switch maintenance off forever.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            storage::track_activity,
        ))
        // Outermost, over every route: it only establishes the failed-auth throttle's view of *who*
        // is calling (the socket peer), which `guards::authenticate` then reads. Routes that never
        // authenticate — `/health`, the HMAC-signed billing webhook — are unaffected by it.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_throttle::source_scope,
        ))
        .with_state(state)
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
