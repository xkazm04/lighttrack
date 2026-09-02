//! `POST` / `GET` / `DELETE /v1/projects/:id/margin-policies` — the standing margin guardrails.
//!
//! Admin-only, and deliberately so: a policy is a standing instruction to *create caps*, which is a
//! strictly larger power than creating one cap. A project key that could mint policies could cap
//! itself out of existence, or cap a sibling customer, without anyone approving the rule.
//!
//! Nothing here evaluates anything. The policies are read by the forecast sweep
//! ([`crate::margin_guardrails`]), which is the single place that turns them into limit rules — so
//! there is exactly one code path from "this customer is losing money" to "this rule exists".

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;

use lighttrack_core::{new_id, MarginPolicy, PolicyAction, PolicyTrigger};

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::state::{spawn_db, AppState};

/// The body of `POST /v1/projects/:id/margin-policies`. `id` and `project_id` are never in it — the
/// server mints the first, and the path names the second.
#[derive(Deserialize)]
pub(crate) struct PolicyReq {
    trigger: PolicyTrigger,
    action: PolicyAction,
    /// Windowed LLM cost a subject must exceed before the policy acts. Default `0` accepts every
    /// subject, which is rarely what an operator wants — see [`MarginPolicy::min_cost_usd`].
    #[serde(default)]
    min_cost_usd: f64,
    #[serde(default = "default_cooldown")]
    cooldown_secs: u64,
    #[serde(default = "default_expiry")]
    expiry_secs: u64,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_cooldown() -> u64 {
    3600
}

fn default_expiry() -> u64 {
    86_400
}

fn default_true() -> bool {
    true
}

impl PolicyReq {
    fn into_policy(self, id: String, project_id: String) -> MarginPolicy {
        MarginPolicy {
            id,
            project_id,
            trigger: self.trigger,
            min_cost_usd: self.min_cost_usd,
            action: self.action,
            cooldown_secs: self.cooldown_secs,
            expiry_secs: self.expiry_secs,
            enabled: self.enabled,
        }
    }
}

pub(crate) async fn create_policy(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<PolicyReq>,
) -> Result<Json<MarginPolicy>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;

    let store = st.store.clone();
    let pid_check = pid.clone();
    if spawn_db(move || store.get_project(&pid_check))
        .await?
        .is_none()
    {
        return Err(ApiError::not_found(format!("project '{pid}' not found")));
    }

    let policy = req.into_policy(new_id(), pid);
    policy.validate().map_err(ApiError::bad_request)?;
    let store = st.store.clone();
    let p2 = policy.clone();
    spawn_db(move || store.create_margin_policy(&p2)).await?;
    Ok(Json(policy))
}

pub(crate) async fn list_policies(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
) -> Result<Json<Vec<MarginPolicy>>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let v = spawn_db(move || store.list_margin_policies(&pid, false)).await?;
    Ok(Json(v))
}

pub(crate) async fn delete_policy(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((_pid, id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let id2 = id.clone();
    if !spawn_db(move || store.delete_margin_policy(&id2)).await? {
        return Err(ApiError::not_found(format!(
            "margin policy '{id}' not found"
        )));
    }
    // The rules this policy created are NOT reaped here. The sweep's reverse pass owns removal —
    // one code path in, one code path out — and it will drop them on its next run because no
    // surviving policy claims their origin. Until then they keep their own `expires_at`, so a
    // deployment with the sweep switched off still sheds them on time.
    Ok(Json(serde_json::json!({ "deleted": id })))
}
