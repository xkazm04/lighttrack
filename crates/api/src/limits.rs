//! Limit rules: evaluation against rolling usage, management, and status reporting.

use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use serde::{Deserialize, Serialize};

use lighttrack_core::{
    new_id, LimitAction, LimitMetric, LimitRule, LimitScope, LimitStatus, LimitWindow,
};
use lighttrack_store::{StoreError, Usage};

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin, resolve_read_project};
use crate::rejections::RejectionStat;
use crate::state::{spawn_db, AppState};

/// Evaluate all enabled limit rules for a project against current rolling usage.
pub(crate) async fn evaluate_project_limits(
    st: &AppState,
    project: &str,
) -> Result<Vec<LimitStatus>, ApiError> {
    let store = st.store.clone();
    let pid = project.to_string();
    let statuses = spawn_db(move || {
        let rules = store.list_limit_rules(&pid, true)?;
        let now = chrono::Utc::now();
        // Compute usage once per distinct (window, scope): a scoped rule reads its own dimension's
        // rolling total, an unscoped rule the project-wide total. This is the read-only status view
        // (no candidate event), so nothing is folded in.
        let mut usage: HashMap<(LimitWindow, Option<LimitScope>), Usage> = HashMap::new();
        let mut out: Vec<LimitStatus> = Vec::with_capacity(rules.len());
        for r in &rules {
            let key = (r.window, r.scope.clone());
            let u = match usage.get(&key) {
                Some(u) => *u,
                None => {
                    let u = match &r.scope {
                        None => store.usage_since(&pid, r.window.since(now))?,
                        Some(s) => store.usage_since_scoped(&pid, r.window.since(now), s)?,
                    };
                    usage.insert(key, u);
                    u
                }
            };
            out.push(r.evaluate(u.metric_value(r.metric)));
        }
        Ok::<_, StoreError>(out)
    })
    .await?;
    Ok(statuses)
}

#[derive(Deserialize)]
pub(crate) struct CreateLimitReq {
    metric: LimitMetric,
    window: LimitWindow,
    threshold: f64,
    #[serde(default)]
    action: LimitAction,
    /// Whether the rule enforces/alerts on creation. Defaults `true`; the old code hardcoded it,
    /// silently ignoring a client that asked for a rule created disabled.
    #[serde(default = "default_true")]
    enabled: bool,
    /// Optional soft-warning fraction in (0,1) — see [`LimitRule::warn_at`].
    #[serde(default)]
    warn_at: Option<f64>,
    /// Optional dimension scope (`{"model":"gpt-4o"}` etc.) — see [`LimitRule::scope`].
    #[serde(default)]
    scope: Option<LimitScope>,
}

fn default_true() -> bool {
    true
}

pub(crate) async fn create_limit(
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
        enabled: req.enabled,
        warn_at: req.warn_at,
        scope: req.scope,
    };
    rule.validate().map_err(ApiError::bad_request)?;
    let store = st.store.clone();
    let r2 = rule.clone();
    spawn_db(move || store.create_limit_rule(&r2)).await?;
    Ok(Json(rule))
}

/// Fields a `PUT /v1/limits/:id` may change. `project_id` is immutable (a rule can't hop projects);
/// everything else is replaced wholesale. `enabled` is honored so a rule can be toggled off/on.
#[derive(Deserialize)]
pub(crate) struct UpdateLimitReq {
    metric: LimitMetric,
    window: LimitWindow,
    threshold: f64,
    #[serde(default)]
    action: LimitAction,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    warn_at: Option<f64>,
    #[serde(default)]
    scope: Option<LimitScope>,
}

pub(crate) async fn update_limit(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<UpdateLimitReq>,
) -> Result<Json<LimitRule>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;

    // Load the existing rule so we keep its (immutable) project_id and can 404 an unknown id.
    let store = st.store.clone();
    let id_get = id.clone();
    let existing = spawn_db(move || store.get_limit_rule(&id_get))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("limit rule '{id}' not found")))?;

    let rule = LimitRule {
        id: existing.id,
        project_id: existing.project_id,
        metric: req.metric,
        window: req.window,
        threshold: req.threshold,
        action: req.action,
        enabled: req.enabled,
        warn_at: req.warn_at,
        scope: req.scope,
    };
    rule.validate().map_err(ApiError::bad_request)?;
    let store = st.store.clone();
    let r2 = rule.clone();
    // The row exists (we just read it); a `false` here means a concurrent delete raced us.
    if !spawn_db(move || store.update_limit_rule(&r2)).await? {
        return Err(ApiError::not_found(format!("limit rule '{}' not found", rule.id)));
    }
    Ok(Json(rule))
}

pub(crate) async fn delete_limit(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let id2 = id.clone();
    if !spawn_db(move || store.delete_limit_rule(&id2)).await? {
        return Err(ApiError::not_found(format!("limit rule '{id}' not found")));
    }
    Ok(Json(serde_json::json!({ "deleted": id })))
}

pub(crate) async fn list_limits(
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

#[derive(Deserialize)]
pub(crate) struct ProjectParam {
    project: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct LimitStatusResp {
    project_id: String,
    throttled: bool,
    statuses: Vec<LimitStatus>,
    /// Rejected-traffic ledger: per (metric, window), the ingest attempts this project's caps have
    /// turned away (429) with their estimated missed cost. **Best-effort and process-local** — held in
    /// memory, reset on restart, rolled off after 24h (rejected events are never stored, since that
    /// would corrupt the usage/cost math the caps are evaluated against). Empty when nothing's been
    /// rejected recently.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    rejected: Vec<RejectionStat>,
}

pub(crate) async fn limits_status(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ProjectParam>,
) -> Result<Json<LimitStatusResp>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?
        .ok_or_else(|| ApiError::bad_request("project is required"))?;
    let statuses = evaluate_project_limits(&st, &project).await?;
    let throttled = statuses.iter().any(|s| s.rejects_ingest());
    let rejected = st.rejections.snapshot(&project, chrono::Utc::now());
    Ok(Json(LimitStatusResp {
        project_id: project,
        throttled,
        statuses,
        rejected,
    }))
}
