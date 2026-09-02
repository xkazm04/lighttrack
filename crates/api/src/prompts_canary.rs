//! `PUT /v1/projects/:id/prompts/:name/canary` — turn the served-version canary on, tune it, or
//! turn it off.
//!
//! A dedicated route rather than another optional field on the link/promote bodies, for one
//! concrete reason: a policy that can move what production serves must be **explicitly** set and
//! explicitly cleared. Folded into `PUT …/prompts/:name` as a `#[serde(default)]` field, every
//! benchmark-link call that omitted it would silently clear the canary — the exact class of quiet
//! action this whole feature exists to remove.
//!
//! Admin-only in both directions, like every other prompt write: enabling `auto_revert` is handing
//! a background sweep permission to change what a deployment serves.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;

use lighttrack_core::{CanaryPolicy, Prompt};

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::prompts::load_prompt;
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct CanaryReq {
    /// The policy to store. `null` (or an omitted body field) **clears** it — the canary stops
    /// measuring and the sweep stops acting on this prompt.
    #[serde(default)]
    canary: Option<CanaryPolicy>,
}

/// Set or clear a prompt's canary policy. Returns the prompt as it now stands, so a caller sees the
/// defaults the policy was filled in with rather than having to re-read them.
pub(crate) async fn set_canary(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((pid, name)): Path<(String, String)>,
    Json(req): Json<CanaryReq>,
) -> Result<Json<Prompt>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let mut prompt = load_prompt(&st, &pid, &name).await?;

    if let Some(policy) = &req.canary {
        // Refuse a policy that could never fire (a label compared with itself, a zero evidence
        // floor, a drop band outside 0..1) here, rather than storing it and having the sweep skip
        // it every tick — a canary configured wrong must fail loudly, not quietly do nothing.
        if let Some(why) = policy.invalid() {
            return Err(ApiError::bad_request(why));
        }
        if !prompt.labels.contains_key(&policy.production_label) {
            return Err(ApiError::bad_request(format!(
                "'{name}' has no '{}' label to measure against — promote one first",
                policy.production_label
            )));
        }
    }

    prompt.canary = req.canary;
    prompt.updated_at = Utc::now();
    let store = st.store.clone();
    let p2 = prompt.clone();
    spawn_db(move || store.update_prompt(&p2)).await?;
    Ok(Json(prompt))
}
