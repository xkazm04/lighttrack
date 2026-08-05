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
    build_digest, CollectiveDigest, RunStat, DEFAULT_MIN_CASES, DIGEST_SCHEMA_VERSION,
};
use lighttrack_store::{Store, StoreError};

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::state::{spawn_db, AppState};

use super::scorecard::run_stat;

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
    let min_cases = q.min_cases.unwrap_or(DEFAULT_MIN_CASES).max(1);

    let store = st.store.clone();
    let (stats, projects_included, projects_excluded) =
        spawn_db(move || gather_run_stats(store.as_ref())).await?;
    let entries = build_digest(&stats, min_cases);
    Ok(Json(CollectiveDigest {
        schema_version: DIGEST_SCHEMA_VERSION,
        contributor_id: st.collective.contributor_id.clone(),
        generated_at: Utc::now(),
        min_cases,
        projects_included,
        projects_excluded,
        entries,
    }))
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
            for run in store.list_benchmark_runs(&b.id)? {
                if let Some(s) = run_stat(&b, &run) {
                    stats.push(s);
                }
            }
        }
    }
    Ok((stats, included, excluded))
}
