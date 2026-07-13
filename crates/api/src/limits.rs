//! Limit rules: evaluation against rolling usage, management, and status reporting.

use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use serde::{Deserialize, Serialize};

use lighttrack_core::{new_id, LimitAction, LimitMetric, LimitRule, LimitStatus, LimitWindow};
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
            .map(|r| r.evaluate(usage[&r.window].metric_value(r.metric)))
            .collect();
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
        enabled: true,
    };
    let store = st.store.clone();
    let r2 = rule.clone();
    spawn_db(move || store.create_limit_rule(&r2)).await?;
    Ok(Json(rule))
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
