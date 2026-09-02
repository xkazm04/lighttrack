//! Limit rules: evaluation against rolling usage, management, and status reporting.

use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use serde::{Deserialize, Serialize};

use lighttrack_core::{
    new_id, Escalation, LimitAction, LimitMetric, LimitRule, LimitScope, LimitStatus, LimitWindow,
    Threshold,
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
        // The same resolution the admission path runs, against the same helper — so the number on
        // the status page and the number in the 429 are one value, not two computations. A backend
        // that cannot serve revenue leaves them unresolved, i.e. inert and labelled `unknown`.
        let resolved =
            lighttrack_store::resolve_thresholds(&rules, now, |since, until| {
                match store.list_revenue_events(Some(&pid), since, until) {
                    Err(StoreError::Unsupported(_)) => Ok(Vec::new()),
                    other => other,
                }
            })?;
        let resolve = lighttrack_store::resolver(&resolved);
        // Compute usage once per distinct (window, scope): a scoped rule reads its own dimension's
        // rolling total, an unscoped rule the project-wide total. This is the read-only status view
        // (no candidate event), so nothing is folded in.
        let mut usage: HashMap<(LimitWindow, Option<LimitScope>), Usage> = HashMap::new();
        let mut out: Vec<LimitStatus> = Vec::with_capacity(rules.len());
        for r in &rules {
            if !r.is_active_at(now) {
                continue; // an expired policy rule is inert; showing it would misreport the caps
            }
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
            // Same evaluator the ingest admission path uses, so the status surface and the 429 can
            // never disagree — including the cost-provenance qualification of a `cost_usd` cap.
            let (threshold, basis) = resolve(r);
            out.push(lighttrack_store::evaluate_rule_resolved(
                r, &u, threshold, basis,
            ));
        }
        Ok::<_, StoreError>(out)
    })
    .await?;
    Ok(statuses)
}

/// The mutable fields of a limit rule — the body of both `POST /v1/projects/:id/limits` (create)
/// and `PUT /v1/limits/:id` (replace wholesale). One struct so the two doors cannot drift: a field
/// accepted on create is accepted on update, with the same default. `id` and `project_id` are never
/// in the body — the server mints the first and a rule cannot hop projects.
#[derive(Deserialize)]
pub(crate) struct LimitReq {
    metric: LimitMetric,
    window: LimitWindow,
    /// A bare number (a fixed cap, exactly as before) **or** an object — `{"pct": 80}` — for a cap
    /// derived from the subject's recognized revenue. `Threshold` is `#[serde(untagged)]`, so both
    /// wire shapes land on one field and an existing client's body is unchanged.
    threshold: Threshold,
    #[serde(default)]
    action: LimitAction,
    /// Whether the rule enforces/alerts. Defaults `true`; the old create path hardcoded it, silently
    /// ignoring a client that asked for a rule created disabled. Honored on update so a rule can be
    /// toggled off/on.
    #[serde(default = "default_true")]
    enabled: bool,
    /// Optional soft-warning fraction in (0,1) — see [`LimitRule::warn_at`].
    #[serde(default)]
    warn_at: Option<f64>,
    /// Optional dimension scope (`{"model":"gpt-4o"}` etc.) — see [`LimitRule::scope`].
    #[serde(default)]
    scope: Option<LimitScope>,
    /// Optional forecast-driven tightening — see [`Escalation`]. The sweep applies and reverses it;
    /// nothing here takes effect without the sweep running.
    #[serde(default)]
    escalation: Option<Escalation>,
}

impl LimitReq {
    /// Materialize the rule under a given identity (minted on create, preserved on update).
    ///
    /// `prior` is the stored rule on an update path, `None` on create. The sweep-owned fields
    /// (`escalated_until`, `origin`, `expires_at`) are carried over from it rather than taken from
    /// the body: they are automation state, not configuration, and letting a plain `PUT` clear them
    /// would let an operator un-expire a policy rule or de-escalate a project by accident — and
    /// would make the guardrail engine lose track of a rule it owns.
    fn into_rule(self, id: String, project_id: String, prior: Option<&LimitRule>) -> LimitRule {
        LimitRule {
            id,
            project_id,
            metric: self.metric,
            window: self.window,
            threshold: self.threshold,
            action: self.action,
            enabled: self.enabled,
            warn_at: self.warn_at,
            scope: self.scope,
            escalation: self.escalation,
            escalated_until: prior.and_then(|p| p.escalated_until),
            origin: prior.and_then(|p| p.origin.clone()),
            expires_at: prior.and_then(|p| p.expires_at),
        }
    }
}

fn default_true() -> bool {
    true
}

pub(crate) async fn create_limit(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<LimitReq>,
) -> Result<Json<LimitRule>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;

    let store = st.store.clone();
    let pid_check = pid.clone();
    if spawn_db(move || store.get_project(&pid_check))
        .await?
        .is_none()
    {
        return Err(ApiError::not_found(format!("project '{pid}' not found")));
    }

    let rule = req.into_rule(new_id(), pid, None);
    rule.validate().map_err(ApiError::bad_request)?;
    let store = st.store.clone();
    let r2 = rule.clone();
    spawn_db(move || store.create_limit_rule(&r2)).await?;
    Ok(Json(rule))
}

pub(crate) async fn update_limit(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<LimitReq>,
) -> Result<Json<LimitRule>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;

    // Load the existing rule so we keep its (immutable) project_id and can 404 an unknown id.
    let store = st.store.clone();
    let id_get = id.clone();
    let existing = spawn_db(move || store.get_limit_rule(&id_get))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("limit rule '{id}' not found")))?;

    let rule = req.into_rule(
        existing.id.clone(),
        existing.project_id.clone(),
        Some(&existing),
    );
    rule.validate().map_err(ApiError::bad_request)?;
    let store = st.store.clone();
    let r2 = rule.clone();
    // The row exists (we just read it); a `false` here means a concurrent delete raced us.
    if !spawn_db(move || store.update_limit_rule(&r2)).await? {
        return Err(ApiError::not_found(format!(
            "limit rule '{}' not found",
            rule.id
        )));
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
    /// Rejected-traffic ledger: per (metric, window, scope), the ingest attempts this project's caps have
    /// turned away (429) with their estimated missed cost. **Best-effort and process-local** — held in
    /// memory, reset on restart, rolled off after 24h (rejected events are never stored, since that
    /// would corrupt the usage/cost math the caps are evaluated against). Empty when nothing's been
    /// rejected recently.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    rejected: Vec<RejectionStat>,
    /// How much of this project's cost caps rests on weak evidence, and what this deployment does
    /// *not* do about it. Always present, so an operator meets these caveats on a calm afternoon
    /// rather than mid-incident.
    cost_basis: CostBasis,
}

/// The honesty block on `/v1/limits/status`: the aggregate cost provenance behind the returned
/// statuses plus the two standing caveats of ingest-time cost stamping.
#[derive(Serialize)]
pub(crate) struct CostBasis {
    /// Calls, across the returned `cost_usd` statuses' windows, whose model was absent from the price
    /// book. They store `$0.00`; the limit path charges them by imputation instead.
    unpriced_calls: i64,
    /// Total imputed (estimated) cost currently folded into those statuses' `current` values.
    imputed_cost_usd: f64,
    /// Of the stored cost in those windows, how much the *client* self-reported rather than us
    /// pricing it from the book.
    client_reported_cost_usd: f64,
    /// `true` when at least one enforcing cost cap has no priced traffic to measure at all, so it is
    /// currently refusing ingest for want of evidence rather than for spend.
    unpriceable: bool,
    /// How many of the returned statuses rest on a threshold derived from recognized revenue rather
    /// than on a number an operator typed. Their `basis` says what each one resolved to.
    derived_thresholds: usize,
    /// Of those, how many could not be resolved at all and are therefore currently **inert** — they
    /// will never breach until revenue for their subject can be measured. An inert guardrail that
    /// looks configured is exactly the thing an operator must not discover mid-incident.
    inert_thresholds: usize,
    /// Standing caveats, in prose, because their absence is the thing an operator would otherwise
    /// discover during an incident.
    notes: Vec<&'static str>,
}

/// Cost provenance rolled up across the `cost_usd` statuses of one project.
fn cost_basis(statuses: &[LimitStatus]) -> CostBasis {
    let mut b = CostBasis {
        unpriced_calls: 0,
        imputed_cost_usd: 0.0,
        client_reported_cost_usd: 0.0,
        unpriceable: false,
        derived_thresholds: 0,
        inert_thresholds: 0,
        notes: vec![
            "A `revenue_share` threshold is resolved at evaluation time from recognized revenue \
             over the rule's own window (the same recognition the /v1/margin rollup uses), so it \
             follows the invoice instead of going stale. Each status reports the figure it \
             resolved against in `basis`.",
            "A derived threshold whose revenue cannot be measured — an un-invoiced customer, or a \
             backend that does not serve revenue — resolves to infinity and never breaches, \
             reported as `basis.kind = \"unknown\"`. A guardrail we cannot measure is inert by \
             design; it is never a guess that could turn into a surprise 429.",
            "Unpriced calls (model absent from the price book) are charged against a cost cap at the \
             mean cost of a priced call in the same window; the estimate is reported per rule in \
             `cost_evidence`, never written onto the event.",
            "WHICH models are unpriced is answered by GET /v1/costs/unpriced, ranked by call count. \
             Adding the rate there with `?fill_unpriced=1` prices the historical rows too, which is \
             the only way the imputed share above ever reaches zero for past traffic.",
            "An enforcing cost cap whose window contains no priced call at all is unpriceable and \
             refuses ingest — add a price for the model, or cap on `calls`/`tokens` instead.",
            "There is no repricing of history: an event's `cost_usd` is stamped once at ingest, so \
             correcting a WRONG price-book entry does not restate spend already inside a window. \
             The cap stays wrong until the window rolls. Only *unpriced* traffic self-corrects, \
             because its charge is imputed at evaluation time.",
        ],
    };
    for s in statuses {
        if s.derived_threshold() {
            b.derived_thresholds += 1;
        }
        if s.inert() {
            b.inert_thresholds += 1;
        }
        if let Some(e) = &s.cost_evidence {
            b.unpriced_calls += e.unpriced_calls;
            b.imputed_cost_usd += e.imputed_cost_usd;
            b.client_reported_cost_usd += e.client_reported_cost_usd;
            b.unpriceable |= e.unpriceable && s.action.enforces();
        }
    }
    b
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
    let cost_basis = cost_basis(&statuses);
    Ok(Json(LimitStatusResp {
        project_id: project,
        throttled,
        statuses,
        rejected,
        cost_basis,
    }))
}
