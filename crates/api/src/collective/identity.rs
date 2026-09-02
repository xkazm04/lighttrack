//! Who a contribution belongs to.
//!
//! Both write routes (`POST /ingest`, `DELETE /contribution`) resolve the acting source through
//! [`resolve_contributor`], which derives the id from a credential the hub *issued* — never from the
//! request body and never from the raw bearer bytes. The hashing helper is shared with
//! [`super::config::Collective::from_env`], which stamps this instance's own preview id.

use axum::http::HeaderMap;
use sha2::{Digest, Sha256};

use crate::auth::{AuthMode, Principal};
use crate::error::ApiError;
use crate::guards::{authenticate, bearer};
use crate::state::{spawn_db, AppState};

/// First 12 hex chars of SHA-256 — opaque and non-reversible, enough to keep contributors distinct.
pub(super) fn opaque(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize()
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Derive a hub-side contributor id from a **verified** credential: `c-` + the first 12 hex of
/// SHA-256 of the credential's stable identifier (an `api_keys.id`, or the admin key). The id is
/// never taken from the request body, and — since only a credential the hub itself issued can reach
/// this function — a poster can neither overwrite a victim's set nor mint unlimited ids to inflate
/// `n_contributors`. See [`resolve_contributor`].
fn derive_contributor_id(credential: &str) -> String {
    format!("c-{}", opaque(credential))
}

/// Resolve the contributing identity behind an ingest request, or refuse.
///
/// **Why this is not just `authenticate`.** `authenticate` is *lenient in dev mode*: it maps any
/// unrecognized bearer string to [`Principal::Dev`]. Hashing the presented token would therefore let
/// one poster on a dev-mode hub mint an unbounded number of distinct contributor ids and walk straight
/// through `min_contributors` — the floor both the k-anonymity guarantee and the "≥2 independent
/// sources" story rest on. So the identity is derived from a credential the hub *issued*, never from
/// the bytes the poster typed:
///   - [`Principal::Project`] — a key the hub minted, **and** whose project carries
///     `collective_opt_in`. That opt-in is the contribution scope: an ordinary ingest key belongs to a
///     project that never consented, so it cannot contribute. Identity = hash of the `api_keys.id`.
///   - [`Principal::Admin`] — the hub operator pushing its own digest. One key, one identity.
///   - [`Principal::Dev`] — no credential at all (or an unrecognized token on a dev-mode hub).
///     Refused, unless `allow_anon`, in which case *every* such poster collapses into the single
///     shared `anonymous` identity — one source, not N, so nothing can be forged from it either.
pub(super) async fn resolve_contributor(
    st: &AppState,
    headers: &HeaderMap,
) -> Result<String, ApiError> {
    match authenticate(st, headers).await? {
        Principal::Project {
            project_id, key_id, ..
        } => {
            let store = st.store.clone();
            let pid = project_id.clone();
            let project = spawn_db(move || store.get_project(&pid)).await?;
            if !project.map(|p| p.collective_opt_in).unwrap_or(false) {
                return Err(ApiError::forbidden(
                    "this key may not contribute: contribution requires a key whose project has \
                     collective_opt_in set — an ordinary ingest key is not a contributor credential",
                ));
            }
            Ok(derive_contributor_id(&key_id))
        }
        Principal::Admin => Ok(derive_contributor_id(
            st.admin_key.as_deref().unwrap_or("admin"),
        )),
        Principal::Dev => {
            if !st.collective.allow_anon {
                let hint = if bearer(headers).is_some() && st.auth_mode == AuthMode::Dev {
                    "the presented token is not a key this hub issued, and a dev-mode hub cannot tell \
                     one unrecognized token from another — min_contributors cannot be enforced against \
                     forged identities, so the contribution is refused"
                } else {
                    "anonymous (keyless) contributions are refused; present a contributor key, or set \
                     LIGHTTRACK_COLLECTIVE_ALLOW_ANON=1 to accept them under one shared identity"
                };
                return Err(ApiError::forbidden(hint));
            }
            tracing::warn!(
                anon_identity = lighttrack_core::collective::ANON_CONTRIBUTOR,
                "accepting an ANONYMOUS collective contribution (LIGHTTRACK_COLLECTIVE_ALLOW_ANON=1) \
                 — every uncredentialed poster shares one identity and overwrites the others' set",
            );
            Ok(lighttrack_core::collective::ANON_CONTRIBUTOR.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_id_is_stable_and_not_the_input() {
        let a = opaque("my-secret-instance-id");
        assert_eq!(a, opaque("my-secret-instance-id"));
        assert_ne!(a, "my-secret-instance-id");
        assert_eq!(a.len(), 12);
    }
}
