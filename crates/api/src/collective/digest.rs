//! `GET /v1/collective/digest` — build *this* instance's privacy-safe digest from its own benchmark
//! run scorecards (admin; a preview of what it would contribute). Never reads `events`.
//!
//! This module owns the endpoint and the **scope** question — which projects consent to contribute.
//! Turning one run into a publishable stat is [`super::scorecard`]'s job.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;

use lighttrack_core::{
    build_digest_counted, CollectiveDigest, RunStat, DEFAULT_MIN_CASES, DIGEST_SCHEMA_VERSION,
};
use lighttrack_store::{Store, StoreError};

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::state::{spawn_db, AppState};

use super::scorecard::run_stat;
use lighttrack_store::Scope as TenantScope;

#[derive(Deserialize)]
pub(crate) struct DigestParams {
    /// k-anonymity floor; defaults to [`DEFAULT_MIN_CASES`]. Clamped to ≥1.
    min_cases: Option<u32>,
}

/// Build this instance's digest from every benchmark run it stores (admin-only — it walks all
/// projects). Returns what `lt collective contribute` would POST to a hub.
pub(crate) async fn get_digest(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DigestParams>,
) -> Result<Json<CollectiveDigest>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    Ok(Json(build_instance_digest(&st, q.min_cases).await?))
}

/// Build this instance's digest — the body `GET /digest` previews and `POST /contribute` sends.
///
/// Shared rather than duplicated because the two must be the *same* digest: the contribution
/// ledger's hash gate compares what was pushed against what was pushed last time, and a preview
/// that differed from the push in any stored field would make the gate compare two things that were
/// never the same object.
pub(crate) async fn build_instance_digest(
    st: &AppState,
    min_cases: Option<u32>,
) -> Result<CollectiveDigest, ApiError> {
    let min_cases = min_cases.unwrap_or(DEFAULT_MIN_CASES).max(1);
    let store = st.store.clone();
    let (stats, projects_included, projects_excluded) =
        spawn_db(move || gather_run_stats(store.as_ref())).await?;
    let (entries, buckets_withheld) = build_digest_counted(&stats, min_cases);
    Ok(CollectiveDigest {
        schema_version: DIGEST_SCHEMA_VERSION,
        contributor_id: st.collective.contributor_id.clone(),
        generated_at: Utc::now(),
        min_cases,
        projects_included,
        projects_excluded,
        buckets_withheld,
        entries,
    })
}

/// Walk the **consenting** projects' benchmarks and reduce each run scorecard to a [`RunStat`].
/// A project contributes only when `collective_opt_in` is set — contribution is an act, not an
/// inheritance, so an NDA'd project sitting next to a dozen internal ones can never ship by accident.
/// Returns `(stats, projects_included, projects_excluded)` so the digest discloses its own scope.
/// Only runs whose model identity is known and that scored ≥1 case contribute.
fn gather_run_stats(store: &dyn Store) -> Result<(Vec<RunStat>, u32, u32), StoreError> {
    let mut stats = Vec::new();
    let (mut included, mut excluded) = (0u32, 0u32);
    for p in store.list_projects()? {
        if !p.collective_opt_in {
            excluded += 1;
            continue;
        }
        included += 1;
        for b in store.list_benchmarks(&p.id)? {
            // Resolved once per benchmark, not per run: which *generation* of the rubric judged
            // these runs is part of what makes two contributors' numbers comparable, and a
            // superseded rubric is a different measurement (see `rubric_fingerprint_of`).
            let rubric_version = match b.rubric_id.as_deref() {
                Some(id) => store
                    .get_rubric(TenantScope::Project(&p.id), id)?
                    .map(|r| r.version),
                None => None,
            };
            for run in store.list_benchmark_runs(TenantScope::Project(&p.id), &b.id)? {
                if let Some(s) = run_stat(&b, &run, rubric_version) {
                    stats.push(s);
                }
            }
        }
    }
    Ok((stats, included, excluded))
}
