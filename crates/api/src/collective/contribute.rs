//! `POST /v1/collective/contribute` — push this instance's digest to a hub, and record what came
//! back (admin).
//!
//! Contribution used to be a two-hop CLI dance (`GET /digest` here → `POST /ingest` there) that left
//! nothing behind. Moving it inside the API buys three things the CLI could not have:
//!
//! 1. **A ledger row per attempt** ([`ContributionRecord`]), so what left the building is a record
//!    rather than terminal scrollback.
//! 2. **A hash gate.** The digest is hashed with [`digest_sha256`] — which excludes `generated_at`
//!    on purpose — and compared against the last row for this hub. Unchanged ⇒ **no HTTP call at
//!    all**. That is what makes a `Contribute` *schedule* safe: a hub with a `min_interval` would
//!    otherwise answer 429 to every unnecessary push, and the operator would learn to ignore it.
//! 3. **A key that is never in the request.** The caller passes `hub_key_ref`, the *name* of an
//!    environment variable; the API resolves it. A schedule row carrying a hub credential would be
//!    a secret at rest in the observability database.

use axum::{extract::State, http::HeaderMap, Json};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use lighttrack_core::{
    digest_sha256, hub_url_hash, new_id, normalize_hub_url, CollectiveDigest, ContributionRecord,
    ContributionStatus,
};

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::http;
use crate::state::{spawn_db, AppState};

use super::digest::build_instance_digest;

/// Default env var consulted when the caller names no `hub_key_ref`.
pub(crate) const DEFAULT_HUB_KEY_ENV: &str = "LIGHTTRACK_COLLECTIVE_HUB_KEY";

#[derive(Deserialize)]
pub(crate) struct ContributeBody {
    /// The hub's base URL. Trailing slashes tolerated.
    pub(crate) hub: String,
    /// The **name of an environment variable** holding the hub key — never the key itself.
    #[serde(default)]
    pub(crate) hub_key_ref: Option<String>,
    #[serde(default)]
    pub(crate) min_cases: Option<u32>,
    /// Push even when the digest is byte-identical to the last one this hub acked. The escape hatch
    /// for "the hub lost its database", which is the one case where re-sending the same body is the
    /// right thing to do.
    #[serde(default)]
    pub(crate) force: bool,
}

#[derive(Serialize)]
pub(crate) struct ContributeAck {
    /// `sent` | `rejected` | `failed` | `skipped`. `skipped` is not a
    /// [`ContributionStatus`] because nothing happened and nothing was recorded — see
    /// [`post_contribute`].
    outcome: &'static str,
    hub_url_hash: String,
    entries: u32,
    projects_included: u32,
    projects_excluded: u32,
    digest_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    contribution_id: Option<String>,
    #[serde(skip_serializing_if = "Value::is_null")]
    ack: Value,
    /// Why nothing was sent, when `outcome` is `skipped`.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// Build → gate → push → record.
///
/// **A skip is not a ledger row.** The ledger says what left the building; a push that did not
/// happen did not. Writing a row for it would also break the gate itself — the "last row for this
/// hub" would become the skip rather than the send.
pub(crate) async fn post_contribute(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ContributeBody>,
) -> Result<Json<ContributeAck>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;

    let hub = normalize_hub_url(&body.hub).to_string();
    if !(hub.starts_with("http://") || hub.starts_with("https://")) {
        return Err(ApiError::bad_request(format!(
            "'hub' must be an absolute http(s) URL, got {hub:?}"
        )));
    }
    let hub_hash = hub_url_hash(&hub);

    let digest = build_instance_digest(&st, body.min_cases).await?;
    let hash = digest_sha256(&digest);
    let entries = digest.entries.len() as u32;

    let base = ContributeAck {
        outcome: "skipped",
        hub_url_hash: hub_hash.clone(),
        entries,
        projects_included: digest.projects_included,
        projects_excluded: digest.projects_excluded,
        digest_sha256: hash.clone(),
        contribution_id: None,
        ack: Value::Null,
        reason: None,
    };

    // Nothing cleared the k-anonymity floor: an empty digest is not a contribution, and pushing one
    // would ask the hub to replace this contributor's whole set with nothing — a silent withdrawal.
    if entries == 0 {
        return Ok(Json(ContributeAck {
            reason: Some(format!(
                "no (model, task) bucket reached the k≥{} floor, so there is nothing to publish; \
                 an empty push would ask the hub to REPLACE this source's set with nothing",
                digest.min_cases
            )),
            ..base
        }));
    }

    if !body.force {
        if let Some(reason) = unchanged_since_last(&st, &hub_hash, &hash).await? {
            return Ok(Json(ContributeAck {
                reason: Some(reason),
                ..base
            }));
        }
    }

    let key = resolve_hub_key(body.hub_key_ref.as_deref());
    let url = format!("{hub}/v1/collective/ingest");
    let answer = http::post_json(&http::client(), &url, key.as_deref(), &digest).await;
    let record = record_of(&hub_hash, &digest, &hash, &answer);

    // The push already happened. A backend that cannot store the ledger row must not turn a
    // contribution that LANDED into a 501 for the caller — that would read as "nothing was sent"
    // about data now sitting on a hub. It is a declared capability gap, said out loud once.
    let store = st.store.clone();
    let to_store = record.clone();
    if let Err(e) = spawn_db(move || store.insert_contribution(&to_store)).await {
        if e.is_unsupported() {
            tracing::warn!(
                hub = %hub_hash,
                "this backend does not serve the contribution ledger: the push went out but was \
                 NOT recorded, so it is not hash-gated and `withdraw --all` will not know about it"
            );
        } else {
            return Err(e);
        }
    }

    let outcome = match record.status {
        ContributionStatus::Sent => "sent",
        ContributionStatus::Rejected => "rejected",
        ContributionStatus::Failed => "failed",
    };
    if record.status != ContributionStatus::Sent {
        tracing::warn!(
            hub = %hub_hash, outcome, status = ?answer.status,
            "collective contribution did not land; the attempt is in the ledger"
        );
    }
    Ok(Json(ContributeAck {
        outcome,
        contribution_id: Some(record.id.clone()),
        ack: record.ack.clone(),
        ..base
    }))
}

/// `Some(reason)` when the last push to this hub carried the same digest, so this one is a no-op.
///
/// Only a **landed** push counts: a rejected or failed attempt means the hub does not have this
/// digest, so the next attempt must actually go out. A backend without the ledger surface answers
/// `Unsupported` — treated here as "no record", i.e. always push, which is exactly the pre-M22
/// behaviour rather than a refusal to contribute at all.
async fn unchanged_since_last(
    st: &AppState,
    hub_hash: &str,
    hash: &str,
) -> Result<Option<String>, ApiError> {
    let store = st.store.clone();
    let h = hub_hash.to_string();
    let last = match spawn_db(move || store.latest_contribution(&h)).await {
        Ok(v) => v,
        Err(e) if e.is_unsupported() => {
            tracing::debug!(
                "this backend does not serve the contribution ledger; the unchanged-digest gate is \
                 off and every push goes out"
            );
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    Ok(last
        .filter(|c| c.status.landed() && c.digest_sha256 == hash)
        .map(|c| {
            format!(
            "the digest is unchanged since the contribution this hub acked at {} — not re-sending, \
             which is what keeps a scheduled push from tripping the hub's min_interval. Pass \
             force=true to send it anyway.",
            c.created_at.to_rfc3339()
        )
        }))
}

/// Resolve the hub key from the **named** environment variable (or the default one). An unset or
/// empty variable yields `None`: a keyless push is a legitimate configuration on a hub that opted
/// into anonymous contributions, and the hub's own refusal is a better error than one invented here.
pub(crate) fn resolve_hub_key(hub_key_ref: Option<&str>) -> Option<String> {
    let name = hub_key_ref
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_HUB_KEY_ENV);
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Turn one hub answer into the row that goes in the ledger.
fn record_of(
    hub_hash: &str,
    digest: &CollectiveDigest,
    hash: &str,
    answer: &http::Answer,
) -> ContributionRecord {
    let parsed: Option<Value> = serde_json::from_str(&answer.body).ok();
    let status = match (answer.status, answer.ok()) {
        (None, _) => ContributionStatus::Failed,
        (Some(_), true) => ContributionStatus::Sent,
        (Some(_), false) => ContributionStatus::Rejected,
    };
    // The ack is kept as the hub sent it when it is JSON, and wrapped when it is not — a hub that
    // answers HTML on an error is exactly when an operator most needs to see what it said.
    let ack = match (&parsed, answer.status) {
        (Some(v), _) => v.clone(),
        (None, Some(s)) => json!({ "http_status": s, "body": answer.body }),
        (None, None) => json!({ "transport_error": answer.body }),
    };
    ContributionRecord {
        id: new_id(),
        hub_url_hash: hub_hash.to_string(),
        // A hub echoes the identity it filed the contribution under; ours is only a preview.
        contributor_id_as_acked: parsed
            .as_ref()
            .and_then(|v| v.get("contributor_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        schema_version: digest.schema_version,
        generated_at: digest.generated_at,
        entries_count: digest.entries.len() as u32,
        projects_included: digest.projects_included,
        projects_excluded: digest.projects_excluded,
        digest_sha256: hash.to_string(),
        ack,
        status,
        created_at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_core::DIGEST_SCHEMA_VERSION;

    fn digest() -> CollectiveDigest {
        CollectiveDigest {
            schema_version: DIGEST_SCHEMA_VERSION,
            contributor_id: "c-me".into(),
            generated_at: Utc::now(),
            min_cases: 5,
            projects_included: 1,
            projects_excluded: 0,
            buckets_withheld: 0,
            entries: Vec::new(),
        }
    }

    fn answer(status: Option<u16>, body: &str) -> http::Answer {
        http::Answer {
            status,
            body: body.to_string(),
        }
    }

    #[test]
    fn a_2xx_is_sent_and_the_hubs_own_identity_is_kept() {
        let d = digest();
        let r = record_of(
            "h-1",
            &d,
            "abc",
            &answer(Some(200), r#"{"accepted":3,"contributor_id":"c-hubside"}"#),
        );
        assert_eq!(r.status, ContributionStatus::Sent);
        assert_eq!(r.ack["accepted"], 3, "the ack is stored verbatim");
        assert_eq!(
            r.contributor_id_as_acked.as_deref(),
            Some("c-hubside"),
            "the HUB's id, not ours: they can legitimately differ, and that is the thing an \
             operator debugging a non-merging row needs"
        );
        assert_eq!(r.digest_sha256, "abc");
    }

    /// The distinction the ledger exists to keep: a hub that answered and refused is not the same
    /// condition as a hub that never answered, and the operator's next move differs.
    #[test]
    fn a_refusal_and_a_transport_failure_are_different_outcomes() {
        let d = digest();
        let rejected = record_of("h-1", &d, "abc", &answer(Some(429), "too soon"));
        assert_eq!(rejected.status, ContributionStatus::Rejected);
        assert_eq!(rejected.ack["http_status"], 429);
        assert_eq!(
            rejected.ack["body"], "too soon",
            "a non-JSON refusal is still preserved, not dropped"
        );

        let failed = record_of("h-1", &d, "abc", &answer(None, "dns error"));
        assert_eq!(failed.status, ContributionStatus::Failed);
        assert_eq!(failed.ack["transport_error"], "dns error");
        assert!(failed.contributor_id_as_acked.is_none());
    }

    #[test]
    fn the_key_is_read_from_the_named_variable_never_from_the_request() {
        let name = format!("LT_TEST_HUB_KEY_{}", std::process::id());
        assert_eq!(resolve_hub_key(Some(&name)), None, "unset ⇒ keyless");
        std::env::set_var(&name, "  secret-key  ");
        assert_eq!(resolve_hub_key(Some(&name)).as_deref(), Some("secret-key"));
        // An empty variable is not a key — a keyless push and an empty bearer are different things
        // to a hub, and the second is always a 401.
        std::env::set_var(&name, "   ");
        assert_eq!(resolve_hub_key(Some(&name)), None);
        std::env::remove_var(&name);
        // A blank ref falls back to the documented default rather than to the empty variable name.
        assert_eq!(resolve_hub_key(Some("  ")), resolve_hub_key(None));
    }
}
