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
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use auth::{AuthMode, Principal};
use lighttrack_core::{
    new_id, ApiKey, LimitAction, LimitMetric, LimitRule, LimitStatus, LimitWindow, LlmEvent,
    PriceBook, Project, Redaction, Score,
};
use lighttrack_store::{CostRow, SqliteStore, Store, StoreError, Usage};

#[derive(Clone)]
struct AppState {
    store: Arc<SqliteStore>,
    prices: Arc<PriceBook>,
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

    let prices = match std::fs::read_to_string(&pricing) {
        Ok(s) => PriceBook::from_json_str(&s).unwrap_or_else(|e| {
            eprintln!("pricing parse error: {e}; using empty book");
            PriceBook::default()
        }),
        Err(_) => {
            eprintln!("pricing file '{pricing}' not found; using empty book");
            PriceBook::default()
        }
    };

    let state = AppState {
        store: Arc::new(SqliteStore::open(&db)?),
        prices: Arc::new(prices),
        auth_mode,
        admin_key,
    };

    println!(
        "lighttrack-api v{} on http://{bind}  (db={db}, {} priced models, auth={:?}, admin_key={})",
        env!("CARGO_PKG_VERSION"),
        state.prices.len(),
        state.auth_mode,
        if state.admin_key.is_some() { "set" } else { "unset" },
    );

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/events", post(post_event).get(get_events))
        .route("/v1/events/:id", get(get_event_by_id))
        .route("/v1/costs", get(get_costs))
        .route("/v1/scores", post(post_score).get(get_scores))
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
    ev.ensure_cost(st.prices.as_ref());

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
