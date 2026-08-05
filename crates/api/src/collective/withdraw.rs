//! `DELETE /v1/collective/contribution` — withdraw this source's contributed entries.
//!
//! The counterpart to [`super::ingest`]: consent stays revocable rather than one-way, so it is
//! authenticated through the same [`super::identity::resolve_contributor`] path — you may withdraw
//! what you could have published.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::state::{spawn_db, AppState};

use super::identity::resolve_contributor;

#[derive(Deserialize)]
pub(crate) struct WithdrawParams {
    /// Admin-only escape hatch: withdraw a *named* source. The point is the contributor that lost its
    /// key — without this, its rows would be unreachable forever. A non-admin may only withdraw itself.
    contributor: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct WithdrawAck {
    contributor_id: String,
    deleted: u64,
}

/// The right to withdraw: `DELETE /v1/collective/contribution` removes every entry a source
/// contributed. Authenticated exactly like ingest — you may withdraw what you could have published —
/// so a contributor can leave the network without asking the hub operator, and consent stays revocable
/// rather than one-way.
pub(crate) async fn delete_contribution(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<WithdrawParams>,
) -> Result<Json<WithdrawAck>, ApiError> {
    let self_id = resolve_contributor(&st, &headers).await?;
    let target = match q
        .contributor
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => self_id,
        Some(other) if other == self_id => self_id,
        Some(other) => {
            ensure_can_admin(&authenticate(&st, &headers).await?)?;
            other.to_string()
        }
    };
    let store = st.store.clone();
    let who = target.clone();
    let deleted = spawn_db(move || store.delete_collective_entries(&who)).await?;
    Ok(Json(WithdrawAck {
        contributor_id: target,
        deleted,
    }))
}
