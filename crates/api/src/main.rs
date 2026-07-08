//! LightTrack API — ingest + query + project/key/limit management, scoring, benchmarks, jobs.
//!
//! This file is wiring only: build the store + price book, build the router, serve. Handlers live
//! in per-domain modules (`events`, `scores`, `prices`, `datasets`, `rubrics`, `benchmarks`,
//! `jobs`, `projects`, `limits`); shared plumbing in `state`, `error`, `guards`, `auth`.
//!
//! Routes:
//!   GET  /health
//!   POST /v1/events                      ingest one event (cost computed; limits evaluated)
//!   GET  /v1/events?project=&limit=
//!   GET  /v1/events/:id
//!   GET  /v1/traces?project=&limit=     list traces (rollups grouped by trace_id)
//!   GET  /v1/traces/:id                 one trace: totals + span tree + scores within it
//!   POST /v1/traces/:id/score           score a whole trace (anchored to its root span)
//!   GET  /v1/costs?project=
//!   GET  /v1/usecases?project=&since=   use-case rollup: usage+cost by name×provider×model, windowed
//!   POST /v1/scores  GET /v1/scores?project=&limit=
//!   GET  /v1/prices  PUT /v1/prices/:provider/:model
//!   .../datasets .../rubrics .../benchmarks .../jobs            (see modules)
//!   POST /v1/projects/:id/prompts  GET /v1/projects/:id/prompts          prompt registry
//!   GET  /v1/projects/:id/prompts/:name?label=production|version=N       runtime fetch by label
//!   POST /v1/projects/:id/prompts/:name/versions                         new version (auto-benchmarks)
//!   POST /v1/projects/:id/prompts/:name/promote                          label promote (regression-gated)
//!   POST /v1/projects  GET /v1/projects   POST /v1/projects/:id/keys
//!   POST /v1/projects/:id/limits  GET /v1/projects/:id/limits
//!   GET  /v1/limits/status?project=      evaluate limits -> throttle flag + per-rule status
//!   POST /v1/relay/tasks                 enqueue a device task (GET ?project=&status=&limit= lists)
//!   GET  /v1/relay/tasks/:id             task status/result (the originating app polls this)
//!   POST /v1/relay/lease                 device: lease due tasks (device key; outbound-only)
//!   POST /v1/relay/tasks/:id/result      device: report succeeded | failed | deferred
//!   POST /v1/revenue                     record revenue (manual / billing sync) for profit tracking
//!   GET  /v1/margin?by=customer|product&since=&until=   revenue − LLM cost rollup
//!   GET  /v1/forecast?project=&by=&horizon=&lookback=   projected spend/budget-breach + margin-erosion + pre-emptive alerts
//!   POST /v1/billing/:provider/webhook?project=   signed Stripe/Polar webhook → revenue (unauth; HMAC)
//!   GET  /v1/collective/digest?min_cases=     build this instance's privacy-safe model digest (admin)
//!   POST /v1/collective/ingest                hub: accept a contributor's digest (gated; off default)
//!   GET  /v1/collective/leaderboard?task_type=&provider=   merged real-world model leaderboard
//!
//! Env: LIGHTTRACK_BIND, LIGHTTRACK_DB, LIGHTTRACK_DATABASE_URL, LIGHTTRACK_PRICING,
//!      LIGHTTRACK_AUTH_MODE (dev|enforced), LIGHTTRACK_ADMIN_KEY,
//!      LIGHTTRACK_RELAY_DEVICE_KEY (bearer key of the enrolled local device — relay lease/result),
//!      LIGHTTRACK_RELAY_FLAT_COST_USD (fixed cost stamped per relay run event; default 1.0),
//!      LIGHTTRACK_ALERT_WEBHOOK / LIGHTTRACK_ALERT_NTFY / LIGHTTRACK_ALERT_COOLDOWN_SECS (see alerts),
//!      LIGHTTRACK_REDACT_INGEST (off | all | csv of project_ids — scrub PII from input/output; see redact),
//!      LIGHTTRACK_COLLECTIVE_ID (opaque source id — hashed before contribution),
//!      LIGHTTRACK_COLLECTIVE_ACCEPT (1|true — this instance is a leaderboard hub; off by default).

mod alerts;
mod auth;
mod benchmarks;
mod billing;
mod collective;
mod datasets;
mod error;
mod events;
mod forecast;
mod guards;
mod idempotency;
mod jobs;
mod limits;
mod prices;
mod projects;
mod prompts;
mod redact;
mod relay;
mod revenue;
mod rubrics;
mod scores;
mod state;
mod traces;

#[cfg(test)]
mod tests_forecast;
#[cfg(test)]
mod tests_ingest;
#[cfg(test)]
mod tests_relay;
#[cfg(test)]
mod tests_traces;

use std::sync::{Arc, RwLock};

use axum::{
    routing::{get, post, put},
    Router,
};

use lighttrack_core::PriceBook;
use lighttrack_store::{SqliteStore, Store};

use auth::AuthMode;
use state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
    let (store, book) = tokio::task::spawn_blocking(
        move || -> anyhow::Result<(Arc<dyn Store + Send + Sync>, PriceBook)> {
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
                let seed = match std::fs::read_to_string(&pricing) {
                    Ok(s) => PriceBook::from_json_str(&s).unwrap_or_else(|e| {
                        eprintln!("pricing parse error: {e}; seeding empty");
                        PriceBook::default()
                    }),
                    Err(_) => {
                        eprintln!("pricing file '{pricing}' not found; seeding empty");
                        PriceBook::default()
                    }
                };
                for row in seed.rows() {
                    store.upsert_price(&row)?;
                }
                eprintln!("seeded {} model prices into the DB", seed.len());
            }
            let book = PriceBook::from_rows(&store.list_prices()?);
            Ok((store, book))
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
    let seen_webhooks = Arc::new(idempotency::SeenWebhooks::new(idempotency::DEFAULT_CAPACITY));
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
    };

    println!(
        "lighttrack-api v{} on http://{bind}  (store={backend}, {n_prices} priced models, auth={:?}, admin_key={}, alerts={alerts_desc}, redact={redact_desc}, billing={billing_desc}, collective={collective_desc})",
        env!("CARGO_PKG_VERSION"),
        state.auth_mode,
        if state.admin_key.is_some() { "set" } else { "unset" },
    );

    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

pub(crate) fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/events", post(events::post_event).get(events::get_events))
        .route("/v1/events/:id", get(events::get_event_by_id))
        .route("/v1/traces", get(traces::list_traces))
        .route("/v1/traces/:id", get(traces::get_trace))
        .route("/v1/traces/:id/score", post(traces::score_trace))
        .route("/v1/costs", get(events::get_costs))
        .route("/v1/usecases", get(events::get_usecases))
        .route("/v1/scores", post(scores::post_score).get(scores::get_scores))
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
        .route("/v1/benchmarks/:id/runs", get(benchmarks::list_benchmark_runs))
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
        .route("/v1/projects/:id/prompts/:name/promote", post(prompts::promote))
        .route("/v1/jobs", get(jobs::list_jobs))
        .route("/v1/jobs/claim", post(jobs::claim_job))
        .route("/v1/jobs/:id", get(jobs::get_job))
        .route("/v1/jobs/:id/progress", post(jobs::job_progress))
        .route("/v1/jobs/:id/finish", post(jobs::job_finish))
        .route("/v1/projects", post(projects::create_project).get(projects::list_projects))
        .route("/v1/projects/:id/keys", post(projects::create_key))
        .route(
            "/v1/projects/:id/limits",
            post(limits::create_limit).get(limits::list_limits),
        )
        .route("/v1/limits/status", get(limits::limits_status))
        .route("/v1/relay/tasks", post(relay::enqueue_task).get(relay::list_tasks))
        .route("/v1/relay/tasks/:id", get(relay::get_task))
        .route("/v1/relay/tasks/:id/result", post(relay::post_result))
        .route("/v1/relay/lease", post(relay::lease_tasks))
        .route("/v1/revenue", post(revenue::post_revenue))
        .route("/v1/margin", get(revenue::get_margin))
        .route("/v1/forecast", get(forecast::get_forecast))
        .route("/v1/billing/:provider/webhook", post(billing::post_webhook))
        .route("/v1/collective/digest", get(collective::get_digest))
        .route("/v1/collective/ingest", post(collective::post_ingest))
        .route("/v1/collective/leaderboard", get(collective::get_leaderboard))
        .with_state(state)
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

async fn health() -> &'static str {
    "ok"
}
