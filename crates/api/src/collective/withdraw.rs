//! `DELETE /v1/collective/contribution` — withdraw this source's contributed entries.
//!
//! The counterpart to [`super::ingest`]: consent stays revocable rather than one-way, so it is
//! authenticated through the same [`super::identity::resolve_contributor`] path — you may withdraw
//! what you could have published.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::state::{spawn_db, AppState};

use super::identity::resolve_contributor;
use super::withdraw_all;

#[derive(Deserialize)]
pub(crate) struct WithdrawParams {
    /// Admin-only escape hatch: withdraw a *named* source. The point is the contributor that lost its
    /// key — without this, its rows would be unreachable forever. A non-admin may only withdraw itself.
    contributor: Option<String>,
    /// Flip the route around: instead of deleting what a source sent **to us**, withdraw what **we**
    /// sent to every hub our ledger says holds a contribution (admin). See [`super::withdraw_all`].
    #[serde(default)]
    all: Option<String>,
    /// With `all=1`: hub base URLs to consider, repeatable. The ledger stores an opaque hash, not an
    /// address, so this is how an operator names a hub the deployment does not otherwise record.
    #[serde(default)]
    hub: Option<String>,
    /// With `all=1`: the **name** of the env var holding the hub key, never the key.
    #[serde(default)]
    hub_key_ref: Option<String>,
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
) -> Result<Response, ApiError> {
    // `all=1` is the contributor-side fan-out, and it is admin-only: it spends this deployment's own
    // credentials against third-party hubs, which is strictly more than "delete my own rows here".
    if truthy(q.all.as_deref()) {
        ensure_can_admin(&authenticate(&st, &headers).await?)?;
        let hubs: Vec<String> = q.hub.iter().cloned().collect();
        let ack = withdraw_all::withdraw_from_all(&st, &hubs, q.hub_key_ref.as_deref()).await?;
        return Ok(Json(ack).into_response());
    }
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
    })
    .into_response())
}

/// `?all=1` / `true` / `on` / `yes`, or a bare `?all` — the same vocabulary the env flags use.
fn truthy(v: Option<&str>) -> bool {
    matches!(
        v.map(str::trim),
        Some("") | Some("1") | Some("true") | Some("on") | Some("yes")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `?all` with no value is how a shell writes a flag; treating it as false would silently do
    /// the OPPOSITE thing (a hub-side self-delete) from what the operator asked.
    #[test]
    fn a_bare_all_flag_counts_as_set() {
        for v in [Some(""), Some("1"), Some("true"), Some(" yes ")] {
            assert!(truthy(v), "{v:?}");
        }
        for v in [None, Some("0"), Some("false"), Some("no")] {
            assert!(!truthy(v), "{v:?}");
        }
    }
}
