//! `DELETE /v1/collective/contribution?all=1` — withdraw from **every hub this instance has
//! contributed to** (admin).
//!
//! The counterpart to [`super::withdraw`], which is the *hub* side: that one deletes what a source
//! sent **to us**. This one is the *contributor* side — it walks our own ledger and asks each hub
//! that acked a contribution to delete it. Before the ledger existed, revoking consent meant the
//! operator remembering every hub URL and key they had ever pushed to, which is exactly the kind of
//! thing nobody remembers under pressure.
//!
//! ## Resolving a hub the ledger only knows by hash
//!
//! [`ContributionRecord::hub_url_hash`] is an opaque key, not an address — so the URLs come from the
//! places this deployment already writes them down, and the ledger decides which of those are
//! *actually holding data*:
//!
//! * `?hub=` (repeatable) — what the operator typed;
//! * every stored `Contribute` **schedule**'s payload — the auto-push's own configuration;
//! * `LIGHTTRACK_COLLECTIVE_HUBS` — a comma-separated list, for a deployment that pushes by hand.
//!
//! A ledgered hub none of those name is **reported, not silently dropped**: `unresolved` in the
//! response is the operator's list of "you contributed here and I cannot name it", which is a far
//! better answer than a withdrawal that quietly covered less than it claimed.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{json, Value};

use lighttrack_core::{hub_url_hash, normalize_hub_url, ContributePayload, JobKind};

use crate::error::ApiError;
use crate::http;
use crate::state::{spawn_db, AppState};

use super::contribute::resolve_hub_key;

/// Env var naming extra hubs to consider, comma-separated.
const ENV_HUBS: &str = "LIGHTTRACK_COLLECTIVE_HUBS";

#[derive(Serialize)]
pub(crate) struct WithdrawAllAck {
    /// One entry per hub actually contacted.
    withdrawn: Vec<HubWithdrawal>,
    /// Hubs the ledger says hold a contribution but whose URL this deployment cannot name. The
    /// operator's to-do list: re-run with `?hub=<url>` for each.
    unresolved: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct HubWithdrawal {
    hub_url_hash: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    response: Value,
}

/// Every hub the ledger says currently holds a landed contribution from this instance.
async fn ledgered_hubs(st: &AppState) -> Result<BTreeSet<String>, ApiError> {
    let store = st.store.clone();
    let rows = spawn_db(move || store.list_contributions(0, None)).await?;
    // Newest-first, so the first row seen for a hub is its current state: a hub whose most recent
    // attempt did not land holds whatever the previous one left, so `landed()` on the newest row is
    // NOT the test — any landed row means the hub has data of ours until we withdraw it.
    Ok(rows
        .into_iter()
        .filter(|c| c.status.landed())
        .map(|c| c.hub_url_hash)
        .collect())
}

/// Hub URLs this deployment can name, indexed by their hash.
async fn known_hubs(st: &AppState, explicit: &[String]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut add = |url: &str| {
        let u = normalize_hub_url(url);
        if u.starts_with("http://") || u.starts_with("https://") {
            out.insert(hub_url_hash(u), u.to_string());
        }
    };
    for u in explicit {
        add(u);
    }
    if let Ok(list) = std::env::var(ENV_HUBS) {
        for u in list.split(',') {
            add(u);
        }
    }
    // The auto-push's own configuration. A `Contribute` schedule is where a deployment that pushes
    // on a timer records which hub it pushes to, so it is the one place the URL reliably lives.
    let store = st.store.clone();
    let schedules = spawn_db(move || {
        let mut all = Vec::new();
        for p in store.list_projects()? {
            all.extend(store.list_schedules(&p.id)?);
        }
        Ok(all)
    })
    .await;
    if let Ok(schedules) = schedules {
        for s in schedules {
            if s.kind != JobKind::Contribute.as_str() {
                continue;
            }
            if let Ok(p) = serde_json::from_value::<ContributePayload>(s.payload.clone()) {
                add(&p.hub);
            }
        }
    }
    out
}

/// Ask every ledgered hub we can name to delete this instance's contribution.
pub(crate) async fn withdraw_from_all(
    st: &AppState,
    explicit: &[String],
    hub_key_ref: Option<&str>,
) -> Result<WithdrawAllAck, ApiError> {
    let ledgered = ledgered_hubs(st).await?;
    let known = known_hubs(st, explicit).await;

    let client = http::client();
    let key = resolve_hub_key(hub_key_ref);
    let mut withdrawn = Vec::new();
    let mut unresolved = Vec::new();
    for hash in ledgered {
        let Some(url) = known.get(&hash) else {
            unresolved.push(hash);
            continue;
        };
        let answer = http::delete(
            &client,
            &format!("{url}/v1/collective/contribution"),
            key.as_deref(),
        )
        .await;
        let response = serde_json::from_str(&answer.body)
            .unwrap_or_else(|_| json!({ "body": answer.body.clone() }));
        if !answer.ok() {
            tracing::warn!(hub = %hash, status = ?answer.status, "withdrawal from a hub did not land");
        }
        withdrawn.push(HubWithdrawal {
            hub_url_hash: hash,
            ok: answer.ok(),
            status: answer.status,
            response,
        });
    }
    if !unresolved.is_empty() {
        tracing::warn!(
            count = unresolved.len(),
            "the ledger names hubs this deployment cannot resolve to a URL; re-run with \
             ?hub=<url> for each, or set {ENV_HUBS}"
        );
    }
    Ok(WithdrawAllAck {
        withdrawn,
        unresolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hash is the join key between the ledger and whatever named the hub, so the two sides
    /// must normalize identically — a trailing slash typed in `?hub=` must still match the row a
    /// push wrote.
    #[test]
    fn a_hub_matches_its_ledger_row_whatever_slashes_were_typed() {
        let pushed = hub_url_hash("https://hub.example");
        assert_eq!(hub_url_hash("https://hub.example/"), pushed);
        assert_eq!(hub_url_hash("  https://hub.example//  "), pushed);
        assert_ne!(hub_url_hash("https://hub.example/sub"), pushed);
    }
}
