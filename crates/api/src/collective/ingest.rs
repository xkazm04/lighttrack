//! `POST /v1/collective/ingest` — a hub receives a digest from a contributor and stores it (gated by
//! `LIGHTTRACK_COLLECTIVE_ACCEPT`; off by default).
//!
//! This module owns the *transaction*: gate, identity, rate limit, replace-the-source's-set, sweep,
//! and the disclosure ack. What makes a single contributed entry acceptable is
//! [`super::sanitize`]'s job.

use axum::{extract::State, http::HeaderMap, Json};
use chrono::Utc;
use serde::Serialize;

use lighttrack_core::{
    CollectiveDigest, CollectiveEntry, DIGEST_SCHEMA_VERSION, MIN_SCHEMA_VERSION,
};
use lighttrack_store::StoreError;

use crate::error::ApiError;
use crate::state::{spawn_db, AppState};

use super::identity::resolve_contributor;
use super::sanitize::{sanitize_entry, Reject};

/// Hard cap on entries accepted from one contributor, so a malformed/abusive digest can't blow up.
const MAX_ENTRIES: usize = 5000;

#[derive(Serialize)]
pub(crate) struct IngestAck {
    /// The **hub-derived** identity this contribution landed under (from the bearer key, not the body).
    contributor_id: String,
    accepted: usize,
    /// Entries dropped as malformed / identity-less (empty provider, model, or task_type).
    skipped: usize,
    /// Entries dropped for failing the hub's enforced k-anonymity floor (`n_cases < min_cases`).
    dropped_under_min: usize,
    /// Entries refused as not-believable benchmark results (see [`super::sanitize::implausible`]) — a
    /// claim of a billion cases is disclosed back to the contributor, never silently absorbed into a
    /// merged row.
    rejected_implausible: usize,
}

/// Hub side: accept a contributor's digest and replace its stored entry set (delete-then-upsert so a
/// bucket that fell below the floor doesn't linger). Off unless `LIGHTTRACK_COLLECTIVE_ACCEPT` is set.
///
/// Hardening: the contributor identity is **derived from a credential the hub issued**, never trusted
/// from the request body — so a poster can only ever replace *its own* set, and cannot mint identities
/// to defeat `min_contributors` (see [`resolve_contributor`]). Contribution needs a key whose project
/// carries `collective_opt_in`; an uncredentialed push is refused unless
/// `LIGHTTRACK_COLLECTIVE_ALLOW_ANON=1`, in which case it lands under one shared `anonymous` identity
/// with a loud warning. The hub also re-enforces its own k-anonymity floor
/// (`LIGHTTRACK_COLLECTIVE_MIN_CASES`), dropping under-k buckets rather than trusting the poster's floor.
pub(crate) async fn post_ingest(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(digest): Json<CollectiveDigest>,
) -> Result<Json<IngestAck>, ApiError> {
    if !st.collective.accept {
        return Err(ApiError::forbidden(
            "this instance does not accept collective contributions (set LIGHTTRACK_COLLECTIVE_ACCEPT=1)",
        ));
    }
    if !(MIN_SCHEMA_VERSION..=DIGEST_SCHEMA_VERSION).contains(&digest.schema_version) {
        return Err(ApiError::bad_request(format!(
            "unsupported digest schema_version {} (this hub accepts v{MIN_SCHEMA_VERSION}..=v{DIGEST_SCHEMA_VERSION})",
            digest.schema_version
        )));
    }

    // Identity comes from a hub-issued credential; the body's `contributor_id` is ignored (wire compat).
    let contributor = resolve_contributor(&st, &headers).await?;

    let min_cases = st.collective.min_cases;
    let now = Utc::now();
    enforce_min_interval(&st, &contributor, now).await?;
    let mut skipped = 0usize;
    let mut dropped_under_min = 0usize;
    let mut rejected_implausible = 0usize;
    let entries: Vec<CollectiveEntry> = digest
        .entries
        .into_iter()
        .filter_map(
            |e| match sanitize_entry(&contributor, e, now, &st.collective.aliases) {
                Err(Reject::Malformed) => {
                    skipped += 1;
                    None
                }
                Err(Reject::Implausible) => {
                    rejected_implausible += 1;
                    None
                }
                Ok(ce) if ce.n_cases < min_cases => {
                    dropped_under_min += 1;
                    None
                }
                Ok(ce) => Some(ce),
            },
        )
        .take(MAX_ENTRIES)
        .collect();
    let accepted = entries.len();

    let store = st.store.clone();
    let contrib = contributor.clone();
    let cutoff = st.collective.retention_cutoff(now);
    spawn_db(move || -> Result<(), StoreError> {
        store.delete_collective_entries(&contrib)?;
        for e in &entries {
            store.upsert_collective_entry(e)?;
        }
        // Retention sweep, piggy-backed on the write that already holds the connection. A backend
        // without a sweep still enforces the policy at read time, so `Unsupported` is not an error.
        if let Some(c) = cutoff {
            match store.purge_collective_entries_before(c) {
                Ok(_) | Err(StoreError::Unsupported(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    })
    .await?;

    Ok(Json(IngestAck {
        contributor_id: contributor,
        accepted,
        skipped,
        dropped_under_min,
        rejected_implausible,
    }))
}

/// Bound how finely a hub operator can difference a contributor over time.
///
/// Ingest is delete-then-replace under a stable source id, so successive pushes are a changelog of a
/// contributor's private benchmark suite: a new `task_type` appearing, a cost moving 30%, a bucket
/// vanishing. Nothing in the payload leaks that — the *sequence* does. A minimum interval makes the
/// changelog coarse; `0` (default) leaves it off, which is why the exposure is also documented for
/// both sides in `docs/BENCHMARK_FRAMEWORK.md` rather than being quietly relied on.
async fn enforce_min_interval(
    st: &AppState,
    contributor: &str,
    now: chrono::DateTime<Utc>,
) -> Result<(), ApiError> {
    let hours = st.collective.min_interval_hours;
    if hours == 0 {
        return Ok(());
    }
    let store = st.store.clone();
    let who = contributor.to_string();
    let last = spawn_db(move || {
        store.list_collective_entries().map(|es| {
            es.iter()
                .filter(|e| e.contributor_id == who)
                .map(|e| e.received_at)
                .max()
        })
    })
    .await?;
    let Some(last) = last else { return Ok(()) };
    let next = last + chrono::Duration::hours(hours as i64);
    if now < next {
        let secs = (next - now).num_seconds().max(1) as u64;
        return Err(ApiError::rate_limited(format!(
            "this hub accepts one contribution per source every {hours}h (frequent re-pushes let a hub \
             operator difference your private benchmark suite); retry in {secs}s"
        ))
        .retry_after(Some(secs)));
    }
    Ok(())
}
