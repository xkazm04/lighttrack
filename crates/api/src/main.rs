//! LightTrack API — ingest + query + project/key/limit management (Phases 1–2).
//!
//! Routes:
//!   GET  /health
//!   POST /v1/events                      ingest one event (cost computed; limits evaluated)
//!   GET  /v1/events?project=&limit=
//!   GET  /v1/costs?project=
//!   POST /v1/projects                    (admin) create a project
//!   GET  /v1/projects                    (admin) list projects
//!   POST /v1/projects/:id/keys           (admin) mint an API key (returned once)
//!   POST /v1/projects/:id/limits         (admin) add a limit rule
//!   GET  /v1/projects/:id/limits
//!   GET  /v1/limits/status?project=      evaluate limits -> throttle flag + per-rule status
//!
//! Env: LIGHTTRACK_BIND, LIGHTTRACK_DB, LIGHTTRACK_PRICING,
//!      LIGHTTRACK_AUTH_MODE (dev|enforced), LIGHTTRACK_ADMIN_KEY.

mod auth;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use auth::{AuthMode, Principal};
use lighttrack_core::{
    new_id, ApiKey, Benchmark, BenchmarkCase, BenchmarkRun, Dataset, DatasetItem, Job, LimitAction,
    LimitMetric, LimitRule, LimitStatus, LimitWindow, LlmEvent, ModelPriceRow, PriceBook, Project,
    Redaction, Rubric, RubricDimension, Score,
};
use lighttrack_store::{CostRow, SqliteStore, Store, StoreError, Usage};

#[derive(Clone)]
struct AppState {
    store: Arc<SqliteStore>,
    /// DB-backed price book, hot-swappable via `PUT /v1/prices/:provider/:model`.
    prices: Arc<RwLock<PriceBook>>,
    auth_mode: AuthMode,
    admin_key: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bind = env_or("LIGHTTRACK_BIND", "127.0.0.1:8787");
    let db = env_or("LIGHTTRACK_DB", "data/lighttrack.db");
    let pricing = env_or("LIGHTTRACK_PRICING", "config/pricing.json");
    let auth_mode = AuthMode::from_env(&env_or("LIGHTTRACK_AUTH_MODE", "dev"));
    let admin_key = std::env::var("LIGHTTRACK_ADMIN_KEY")
        .ok()
        .filter(|s| !s.is_empty());

    let store = Arc::new(SqliteStore::open(&db)?);

    // Seed the DB price book from pricing.json on first run; thereafter the DB is the source of truth.
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
    let n_prices = book.len();

    let state = AppState {
        store,
        prices: Arc::new(RwLock::new(book)),
        auth_mode,
        admin_key,
    };

    println!(
        "lighttrack-api v{} on http://{bind}  (db={db}, {n_prices} priced models, auth={:?}, admin_key={})",
        env!("CARGO_PKG_VERSION"),
        state.auth_mode,
        if state.admin_key.is_some() { "set" } else { "unset" },
    );

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/events", post(post_event).get(get_events))
        .route("/v1/events/:id", get(get_event_by_id))
        .route("/v1/costs", get(get_costs))
        .route("/v1/scores", post(post_score).get(get_scores))
        .route("/v1/prices", get(get_prices))
        .route("/v1/prices/:provider/:model", put(put_price))
        .route(
            "/v1/projects/:id/datasets",
            post(create_dataset).get(list_datasets),
        )
        .route("/v1/datasets/:id", get(get_dataset))
        .route(
            "/v1/datasets/:id/items",
            post(add_dataset_item).get(list_dataset_items),
        )
        .route("/v1/datasets/:id/freeze", post(freeze_dataset))
        .route(
            "/v1/projects/:id/rubrics",
            post(create_rubric).get(list_rubrics),
        )
        .route("/v1/rubrics/:id", get(get_rubric))
        .route(
            "/v1/projects/:id/benchmarks",
            post(create_benchmark).get(list_benchmarks),
        )
        .route("/v1/benchmarks/:id", get(get_benchmark))
        .route("/v1/benchmarks/:id/runs", get(list_benchmark_runs))
        .route("/v1/benchmark-runs", post(post_benchmark_run))
        .route("/v1/benchmarks/:id/enqueue", post(enqueue_benchmark))
        .route("/v1/jobs", get(list_jobs))
        .route("/v1/jobs/claim", post(claim_job))
        .route("/v1/jobs/:id", get(get_job))
        .route("/v1/jobs/:id/progress", post(job_progress))
        .route("/v1/jobs/:id/finish", post(job_finish))
        .route("/v1/projects", post(create_project).get(list_projects))
        .route("/v1/projects/:id/keys", post(create_key))
        .route("/v1/projects/:id/limits", post(create_limit).get(list_limits))
        .route("/v1/limits/status", get(limits_status))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

async fn health() -> &'static str {
    "ok"
}

// ----------------------------------------------------------------------------
// Auth helpers
// ----------------------------------------------------------------------------

fn bearer(headers: &HeaderMap) -> Option<String> {
    let h = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let rest = h
        .strip_prefix("Bearer ")
        .or_else(|| h.strip_prefix("bearer "))?;
    Some(rest.trim().to_string())
}

/// Resolve the principal behind a request (see `auth` module for mode semantics).
async fn authenticate(st: &AppState, headers: &HeaderMap) -> Result<Principal, ApiError> {
    let token = match bearer(headers) {
        Some(t) => t,
        None => {
            return match st.auth_mode {
                AuthMode::Dev => Ok(Principal::Dev),
                AuthMode::Enforced => Err(ApiError::unauthorized("missing API key")),
            }
        }
    };

    if let Some(admin) = &st.admin_key {
        if &token == admin {
            return Ok(Principal::Admin);
        }
    }

    if let Some(prefix) = auth::prefix_of(&token) {
        let store = st.store.clone();
        let key = spawn_db(move || store.find_api_key_by_prefix(&prefix)).await?;
        if let Some(k) = key {
            if !k.revoked && auth::verify_key(&k.key_hash, &token) {
                // Best-effort, detached: record last use without delaying the request.
                let store2 = st.store.clone();
                let id = k.id.clone();
                tokio::spawn(async move {
                    let _ =
                        tokio::task::spawn_blocking(move || store2.touch_api_key(&id, Utc::now()))
                            .await;
                });
                return Ok(Principal::Project(k.project_id));
            }
        }
    }

    match st.auth_mode {
        AuthMode::Dev => Ok(Principal::Dev), // lenient in dev: ignore an unrecognized token
        AuthMode::Enforced => Err(ApiError::unauthorized("invalid API key")),
    }
}

fn ensure_can_admin(p: &Principal) -> Result<(), ApiError> {
    match p {
        Principal::Admin | Principal::Dev => Ok(()),
        Principal::Project(_) => Err(ApiError::forbidden("admin privileges required")),
    }
}

/// Which project an ingested event belongs to. A project key forces its own project.
fn resolve_ingest_project(p: &Principal, body_project: &str) -> Result<String, ApiError> {
    match p {
        Principal::Project(pid) => Ok(pid.clone()),
        Principal::Admin | Principal::Dev => {
            if body_project.trim().is_empty() {
                Err(ApiError::bad_request("project_id is required"))
            } else {
                Ok(body_project.to_string())
            }
        }
    }
}

/// Which project a read may target. A project key may only read its own project.
fn resolve_read_project(p: &Principal, requested: Option<&str>) -> Result<Option<String>, ApiError> {
    match p {
        Principal::Project(pid) => {
            if let Some(r) = requested {
                if r != pid {
                    return Err(ApiError::forbidden("key not authorized for that project"));
                }
            }
            Ok(Some(pid.clone()))
        }
        Principal::Admin | Principal::Dev => Ok(requested.map(str::to_string)),
    }
}

// ----------------------------------------------------------------------------
// Limit evaluation
// ----------------------------------------------------------------------------

/// Evaluate all enabled limit rules for a project against current rolling usage.
async fn evaluate_project_limits(
    st: &AppState,
    project: &str,
) -> Result<Vec<LimitStatus>, ApiError> {
    let store = st.store.clone();
    let pid = project.to_string();
    let statuses = spawn_db(move || {
        let rules = store.list_limit_rules(&pid, true)?;
        let now = Utc::now();
        // Compute usage once per distinct window.
        let mut usage: HashMap<LimitWindow, Usage> = HashMap::new();
        for r in &rules {
            if !usage.contains_key(&r.window) {
                let u = store.usage_since(&pid, r.window.since(now))?;
                usage.insert(r.window, u);
            }
        }
        let out: Vec<LimitStatus> = rules
            .iter()
            .map(|r| {
                let u = usage[&r.window];
                let value = match r.metric {
                    LimitMetric::CostUsd => u.cost_usd,
                    LimitMetric::Calls => u.calls as f64,
                    LimitMetric::Tokens => u.tokens as f64,
                };
                r.evaluate(value)
            })
            .collect();
        Ok::<_, StoreError>(out)
    })
    .await?;
    Ok(statuses)
}

fn is_throttle(s: &LimitStatus) -> bool {
    s.breached && matches!(s.action, LimitAction::Throttle | LimitAction::Block)
}

// ----------------------------------------------------------------------------
// Ingest + query
// ----------------------------------------------------------------------------

#[derive(Serialize)]
struct IngestResponse {
    id: String,
    project_id: String,
    cost_usd: Option<f64>,
    ts: DateTime<Utc>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    breached: Vec<LimitStatus>,
    throttled: bool,
}

async fn post_event(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(mut ev): Json<LlmEvent>,
) -> Result<Json<IngestResponse>, ApiError> {
    let principal = authenticate(&st, &headers).await?;
    let pid = resolve_ingest_project(&principal, &ev.project_id)?;
    ev.project_id = pid.clone();
    {
        let book = st.prices.read().unwrap();
        ev.ensure_cost(&book);
    }

    let store = st.store.clone();
    let to_insert = ev.clone();
    spawn_db(move || store.insert_event(&to_insert)).await?;

    let statuses = evaluate_project_limits(&st, &pid).await?;
    let breached: Vec<LimitStatus> = statuses.into_iter().filter(|s| s.breached).collect();
    let throttled = breached.iter().any(is_throttle);
    for b in &breached {
        eprintln!(
            "[ALERT] project={} metric={:?} window={:?} value={:.6} >= threshold={:.6} action={:?}",
            b.project_id, b.metric, b.window, b.current, b.threshold, b.action
        );
    }

    Ok(Json(IngestResponse {
        id: ev.id,
        project_id: pid,
        cost_usd: ev.cost_usd,
        ts: ev.ts,
        breached,
        throttled,
    }))
}

#[derive(Deserialize)]
struct EventsParams {
    project: Option<String>,
    limit: Option<usize>,
}

async fn get_events(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<EventsParams>,
) -> Result<Json<Vec<LlmEvent>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let store = st.store.clone();
    let limit = q.limit.unwrap_or(50).min(1000);
    let events = spawn_db(move || store.list_events(project.as_deref(), limit)).await?;
    Ok(Json(events))
}

#[derive(Deserialize)]
struct ProjectParam {
    project: Option<String>,
}

async fn get_costs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ProjectParam>,
) -> Result<Json<Vec<CostRow>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let store = st.store.clone();
    let rows = spawn_db(move || store.cost_summary(project.as_deref())).await?;
    Ok(Json(rows))
}

async fn get_event_by_id(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<LlmEvent>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let store = st.store.clone();
    let id2 = id.clone();
    let ev = spawn_db(move || store.get_event(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("event '{id}' not found")))?;
    if let Principal::Project(pid) = &p {
        if &ev.project_id != pid {
            return Err(ApiError::forbidden("key not authorized for that event's project"));
        }
    }
    Ok(Json(ev))
}

// ----------------------------------------------------------------------------
// Scores (Phase 3) — the runner posts judge verdicts here; clients read them back
// ----------------------------------------------------------------------------

async fn post_score(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(mut s): Json<Score>,
) -> Result<Json<Score>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    s.project_id = resolve_ingest_project(&p, &s.project_id)?;
    let store = st.store.clone();
    let s2 = s.clone();
    spawn_db(move || store.insert_score(&s2)).await?;
    Ok(Json(s))
}

#[derive(Deserialize)]
struct ScoresParams {
    project: Option<String>,
    limit: Option<usize>,
}

async fn get_scores(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ScoresParams>,
) -> Result<Json<Vec<Score>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let store = st.store.clone();
    let limit = q.limit.unwrap_or(50).min(1000);
    let scores = spawn_db(move || store.list_scores(project.as_deref(), limit)).await?;
    Ok(Json(scores))
}

// ----------------------------------------------------------------------------
// Benchmarks (Phase 3.5)
// ----------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateBenchmarkReq {
    name: String,
    /// Freeform rubric text (single-score mode); optional when `rubric_id` is set.
    #[serde(default)]
    rubric: String,
    #[serde(default = "default_judge_model")]
    judge_model: String,
    #[serde(default)]
    target: serde_json::Value,
    #[serde(default)]
    dataset: Vec<BenchmarkCase>,
    /// Reference a stored dataset by id instead of (or in addition to) an inline dataset.
    #[serde(default)]
    dataset_ref: Option<String>,
    /// Optional structured rubric (id) for per-dimension judging.
    #[serde(default)]
    rubric_id: Option<String>,
    #[serde(default)]
    baseline_score: Option<f64>,
}

fn default_judge_model() -> String {
    "haiku".to_string()
}

async fn create_benchmark(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<CreateBenchmarkReq>,
) -> Result<Json<Benchmark>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let b = Benchmark {
        id: new_id(),
        project_id: pid,
        name: req.name,
        rubric: req.rubric,
        judge_model: req.judge_model,
        target: req.target,
        dataset_ref: req.dataset_ref,
        dataset: req.dataset,
        rubric_id: req.rubric_id,
        baseline_score: req.baseline_score,
        created_at: Utc::now(),
    };
    let store = st.store.clone();
    let b2 = b.clone();
    spawn_db(move || store.create_benchmark(&b2)).await?;
    Ok(Json(b))
}

async fn list_benchmarks(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
) -> Result<Json<Vec<Benchmark>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    resolve_read_project(&p, Some(&pid))?;
    let store = st.store.clone();
    let v = spawn_db(move || store.list_benchmarks(&pid)).await?;
    Ok(Json(v))
}

/// Fetch a benchmark and authorize project-key access to it.
async fn load_benchmark_authorized(
    st: &AppState,
    p: &Principal,
    id: &str,
) -> Result<Benchmark, ApiError> {
    let store = st.store.clone();
    let id2 = id.to_string();
    let bench = spawn_db(move || store.get_benchmark(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("benchmark '{id}' not found")))?;
    if let Principal::Project(pid) = p {
        if &bench.project_id != pid {
            return Err(ApiError::forbidden("key not authorized for that benchmark"));
        }
    }
    Ok(bench)
}

async fn get_benchmark(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Benchmark>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    Ok(Json(load_benchmark_authorized(&st, &p, &id).await?))
}

async fn list_benchmark_runs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Vec<BenchmarkRun>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    load_benchmark_authorized(&st, &p, &id).await?; // authorize
    let store = st.store.clone();
    let runs = spawn_db(move || store.list_benchmark_runs(&id)).await?;
    Ok(Json(runs))
}

async fn post_benchmark_run(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(run): Json<BenchmarkRun>,
) -> Result<Json<BenchmarkRun>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    load_benchmark_authorized(&st, &p, &run.benchmark_id).await?; // authorize via the benchmark
    let store = st.store.clone();
    let run2 = run.clone();
    spawn_db(move || store.create_benchmark_run(&run2)).await?;
    Ok(Json(run))
}

// ----------------------------------------------------------------------------
// Job queue (Phase 3.6d) — enqueue returns immediately; lt-runner serve executes
// ----------------------------------------------------------------------------

#[derive(Deserialize)]
struct EnqueueReq {
    #[serde(default = "default_samples")]
    samples: u32,
    #[serde(default)]
    heal: bool,
}

fn default_samples() -> u32 {
    1
}

async fn enqueue_benchmark(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<EnqueueReq>,
) -> Result<Json<Job>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let bench = load_benchmark_authorized(&st, &p, &id).await?;
    let job = Job {
        id: new_id(),
        job_type: "bench_run".to_string(),
        payload: serde_json::json!({ "benchmark_id": bench.id, "samples": req.samples, "heal": req.heal }),
        status: "queued".to_string(),
        attempts: 0,
        max_attempts: 3,
        progress: None,
        error: None,
        result: serde_json::Value::Null,
        claimed_at: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let store = st.store.clone();
    let j2 = job.clone();
    spawn_db(move || store.create_job(&j2)).await?;
    Ok(Json(job))
}

#[derive(Deserialize)]
struct JobsParams {
    status: Option<String>,
    limit: Option<usize>,
}

async fn list_jobs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<JobsParams>,
) -> Result<Json<Vec<Job>>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let status = q.status;
    let limit = q.limit.unwrap_or(50).min(1000);
    let jobs = spawn_db(move || store.list_jobs(status.as_deref(), limit)).await?;
    Ok(Json(jobs))
}

async fn get_job(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Job>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let id2 = id.clone();
    let job = spawn_db(move || store.get_job(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("job '{id}' not found")))?;
    Ok(Json(job))
}

#[derive(Deserialize)]
struct ClaimReq {
    #[serde(default = "default_stale_secs")]
    stale_secs: i64,
}

fn default_stale_secs() -> i64 {
    600
}

async fn claim_job(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ClaimReq>,
) -> Result<Json<Option<Job>>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let stale_before = Utc::now() - chrono::Duration::seconds(req.stale_secs.max(0));
    let store = st.store.clone();
    let job = spawn_db(move || store.claim_job(stale_before)).await?;
    Ok(Json(job))
}

#[derive(Deserialize)]
struct ProgressReq {
    progress: String,
}

async fn job_progress(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<ProgressReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    spawn_db(move || store.update_job_progress(&id, &req.progress)).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct FinishReq {
    status: String,
    #[serde(default)]
    result: serde_json::Value,
    #[serde(default)]
    error: Option<String>,
}

async fn job_finish(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<FinishReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    spawn_db(move || store.finish_job(&id, &req.status, &req.result, req.error.as_deref())).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ----------------------------------------------------------------------------
// Model prices (Phase 3.6a) — DB-backed, hot-swappable
// ----------------------------------------------------------------------------

async fn get_prices(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ModelPriceRow>>, ApiError> {
    authenticate(&st, &headers).await?;
    let store = st.store.clone();
    let rows = spawn_db(move || store.list_prices()).await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
struct PutPriceReq {
    input_per_mtok: f64,
    output_per_mtok: f64,
    #[serde(default)]
    cached_input_per_mtok: Option<f64>,
    #[serde(default)]
    source_url: Option<String>,
}

async fn put_price(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((provider, model)): Path<(String, String)>,
    Json(req): Json<PutPriceReq>,
) -> Result<Json<ModelPriceRow>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let row = ModelPriceRow {
        provider,
        model,
        input_per_mtok: req.input_per_mtok,
        output_per_mtok: req.output_per_mtok,
        cached_input_per_mtok: req.cached_input_per_mtok,
        effective_date: Utc::now(),
        source_url: req.source_url,
    };
    let store = st.store.clone();
    let row2 = row.clone();
    spawn_db(move || store.upsert_price(&row2)).await?;

    // Hot-swap the in-memory price book so new prices take effect without a restart.
    let store2 = st.store.clone();
    let rows = spawn_db(move || store2.list_prices()).await?;
    {
        let mut book = st.prices.write().unwrap();
        *book = PriceBook::from_rows(&rows);
    }
    Ok(Json(row))
}

// ----------------------------------------------------------------------------
// Datasets (Phase 3.6b)
// ----------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateDatasetReq {
    name: String,
    #[serde(default)]
    source: Option<String>,
}

async fn create_dataset(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<CreateDatasetReq>,
) -> Result<Json<Dataset>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let d = Dataset {
        id: new_id(),
        project_id: pid,
        name: req.name,
        version: 1,
        frozen: false,
        source: req.source,
        created_at: Utc::now(),
    };
    let store = st.store.clone();
    let d2 = d.clone();
    spawn_db(move || store.create_dataset(&d2)).await?;
    Ok(Json(d))
}

async fn list_datasets(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
) -> Result<Json<Vec<Dataset>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    resolve_read_project(&p, Some(&pid))?;
    let store = st.store.clone();
    let v = spawn_db(move || store.list_datasets(&pid)).await?;
    Ok(Json(v))
}

async fn load_dataset_authorized(
    st: &AppState,
    p: &Principal,
    id: &str,
) -> Result<Dataset, ApiError> {
    let store = st.store.clone();
    let id2 = id.to_string();
    let d = spawn_db(move || store.get_dataset(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("dataset '{id}' not found")))?;
    if let Principal::Project(pid) = p {
        if &d.project_id != pid {
            return Err(ApiError::forbidden("key not authorized for that dataset"));
        }
    }
    Ok(d)
}

async fn get_dataset(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Dataset>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    Ok(Json(load_dataset_authorized(&st, &p, &id).await?))
}

async fn add_dataset_item(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(mut item): Json<DatasetItem>,
) -> Result<Json<DatasetItem>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let ds = load_dataset_authorized(&st, &p, &id).await?;
    if ds.frozen {
        return Err(ApiError::new(StatusCode::CONFLICT, "dataset is frozen"));
    }
    item.dataset_id = id;
    let store = st.store.clone();
    let item2 = item.clone();
    spawn_db(move || store.create_dataset_item(&item2)).await?;
    Ok(Json(item))
}

async fn list_dataset_items(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Vec<DatasetItem>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    load_dataset_authorized(&st, &p, &id).await?;
    let store = st.store.clone();
    let items = spawn_db(move || store.list_dataset_items(&id)).await?;
    Ok(Json(items))
}

async fn freeze_dataset(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Dataset>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let mut ds = load_dataset_authorized(&st, &p, &id).await?;
    let store = st.store.clone();
    let id2 = id.clone();
    spawn_db(move || store.set_dataset_frozen(&id2, true)).await?;
    ds.frozen = true;
    Ok(Json(ds))
}

// ----------------------------------------------------------------------------
// Rubrics (Phase 3.6c)
// ----------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateRubricReq {
    name: String,
    dimensions: Vec<RubricDimension>,
    #[serde(default = "default_rubric_threshold")]
    threshold: f64,
}

fn default_rubric_threshold() -> f64 {
    0.7
}

async fn create_rubric(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<CreateRubricReq>,
) -> Result<Json<Rubric>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let r = Rubric {
        id: new_id(),
        project_id: pid,
        name: req.name,
        dimensions: req.dimensions,
        threshold: req.threshold,
        created_at: Utc::now(),
    };
    let store = st.store.clone();
    let r2 = r.clone();
    spawn_db(move || store.create_rubric(&r2)).await?;
    Ok(Json(r))
}

async fn list_rubrics(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
) -> Result<Json<Vec<Rubric>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    resolve_read_project(&p, Some(&pid))?;
    let store = st.store.clone();
    let v = spawn_db(move || store.list_rubrics(&pid)).await?;
    Ok(Json(v))
}

async fn get_rubric(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Rubric>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let store = st.store.clone();
    let id2 = id.clone();
    let r = spawn_db(move || store.get_rubric(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("rubric '{id}' not found")))?;
    if let Principal::Project(pid) = &p {
        if &r.project_id != pid {
            return Err(ApiError::forbidden("key not authorized for that rubric"));
        }
    }
    Ok(Json(r))
}

// ----------------------------------------------------------------------------
// Projects / keys / limits management
// ----------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateProjectReq {
    name: String,
    #[serde(default)]
    redaction: Redaction,
}

async fn create_project(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateProjectReq>,
) -> Result<Json<Project>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let proj = Project {
        id: new_id(),
        name: req.name,
        enabled: true,
        redaction: req.redaction,
        created_at: Utc::now(),
    };
    let store = st.store.clone();
    let pc = proj.clone();
    spawn_db(move || store.create_project(&pc)).await?;
    Ok(Json(proj))
}

async fn list_projects(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Project>>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let v = spawn_db(move || store.list_projects()).await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
struct CreateKeyReq {
    #[serde(default = "default_key_name")]
    name: String,
}

fn default_key_name() -> String {
    "default".to_string()
}

#[derive(Serialize)]
struct CreateKeyResp {
    id: String,
    project_id: String,
    name: String,
    prefix: String,
    /// The full secret — shown exactly once.
    key: String,
    created_at: DateTime<Utc>,
}

async fn create_key(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<CreateKeyReq>,
) -> Result<Json<CreateKeyResp>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;

    let store = st.store.clone();
    let pid_check = pid.clone();
    if spawn_db(move || store.get_project(&pid_check)).await?.is_none() {
        return Err(ApiError::not_found(format!("project '{pid}' not found")));
    }

    let generated = auth::generate_key();
    let now = Utc::now();
    let key = ApiKey {
        id: new_id(),
        project_id: pid.clone(),
        name: req.name,
        prefix: generated.prefix.clone(),
        key_hash: generated.key_hash,
        created_at: now,
        last_used_at: None,
        revoked: false,
    };

    let store = st.store.clone();
    let key2 = key.clone();
    spawn_db(move || store.create_api_key(&key2)).await?;

    Ok(Json(CreateKeyResp {
        id: key.id,
        project_id: pid,
        name: key.name,
        prefix: generated.prefix,
        key: generated.full_key,
        created_at: now,
    }))
}

#[derive(Deserialize)]
struct CreateLimitReq {
    metric: LimitMetric,
    window: LimitWindow,
    threshold: f64,
    #[serde(default)]
    action: LimitAction,
}

async fn create_limit(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<CreateLimitReq>,
) -> Result<Json<LimitRule>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;

    let store = st.store.clone();
    let pid_check = pid.clone();
    if spawn_db(move || store.get_project(&pid_check)).await?.is_none() {
        return Err(ApiError::not_found(format!("project '{pid}' not found")));
    }

    let rule = LimitRule {
        id: new_id(),
        project_id: pid,
        metric: req.metric,
        window: req.window,
        threshold: req.threshold,
        action: req.action,
        enabled: true,
    };
    let store = st.store.clone();
    let r2 = rule.clone();
    spawn_db(move || store.create_limit_rule(&r2)).await?;
    Ok(Json(rule))
}

async fn list_limits(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
) -> Result<Json<Vec<LimitRule>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    resolve_read_project(&p, Some(&pid))?; // authorize project access
    let store = st.store.clone();
    let v = spawn_db(move || store.list_limit_rules(&pid, false)).await?;
    Ok(Json(v))
}

#[derive(Serialize)]
struct LimitStatusResp {
    project_id: String,
    throttled: bool,
    statuses: Vec<LimitStatus>,
}

async fn limits_status(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ProjectParam>,
) -> Result<Json<LimitStatusResp>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?
        .ok_or_else(|| ApiError::bad_request("project is required"))?;
    let statuses = evaluate_project_limits(&st, &project).await?;
    let throttled = statuses.iter().any(is_throttle);
    Ok(Json(LimitStatusResp {
        project_id: project,
        throttled,
        statuses,
    }))
}

// ----------------------------------------------------------------------------
// Plumbing
// ----------------------------------------------------------------------------

/// Run a blocking store call on the blocking pool and flatten the two error layers.
async fn spawn_db<T, F>(f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, StoreError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(format!("task join error: {e}")))?
        .map_err(ApiError::from)
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, m: impl Into<String>) -> Self {
        Self {
            status,
            message: m.into(),
        }
    }
    fn internal(m: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, m)
    }
    fn bad_request(m: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, m)
    }
    fn unauthorized(m: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, m)
    }
    fn forbidden(m: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, m)
    }
    fn not_found(m: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, m)
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        ApiError::internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(serde_json::json!({ "error": self.message }))).into_response()
    }
}
