//! `GET /v1/collective/leaderboard` — the merged public leaderboard across all contributors.
//!
//! Assembly order is load-bearing and is the reason this lives on its own: read → retention →
//! merge → **k-anonymity over sources** → user filters → counts over what survived. A filter that
//! ran before the source floor could strip a row down to one contributor's private eval results.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use lighttrack_core::{merge_leaderboard, LeaderboardRow};

use crate::error::ApiError;
use crate::guards::authenticate;
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct LeaderboardParams {
    /// Filter to one task-type bucket (e.g. `qa`, `summarization`).
    task_type: Option<String>,
    /// Filter to one provider (e.g. `anthropic`).
    provider: Option<String>,
    /// Filter to rows scored (at least partly) by one judge family (`anthropic|openai|google|unknown`).
    judge: Option<String>,
    /// Rigor filter — keep only rows whose **weakest** determinism stamp is this level, i.e. rows
    /// where *every* source ran at that level or better is expressed by asking for the level itself
    /// (`?determinism=exact` ⇒ every source was exact). An unknown label matches nothing.
    determinism: Option<String>,
    /// Rigor filter — `true` keeps only rows whose every source ran against a frozen, single-version
    /// dataset (`frozen_dataset = all`); `false` keeps rows that are anything less than that.
    frozen_dataset: Option<bool>,
    /// Rigor filter — `true` keeps only rows whose every source carried a significance-tested verdict.
    significance_tested: Option<bool>,
}

#[derive(Serialize)]
pub(crate) struct LeaderboardResponse {
    /// Distinct contributing instances **backing the visible rows** — computed over the filtered row
    /// set, so it never disagrees with what's shown. A filter that excludes a contributor's only rows
    /// drops it from this count.
    contributors: usize,
    /// Distinct `(provider, model)` identities across the filtered rows — a true model count, not a row
    /// count. (A single model spans multiple rows when it appears under several task types.)
    n_models: usize,
    /// Number of visible leaderboard rows after filtering (one per `(provider, model, task_type)`).
    n_rows: usize,
    /// Rows withheld for having fewer than the hub's `min_contributors` distinct sources — disclosed
    /// rather than silently shrinking the board, so an empty/short board is legible.
    held_back: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_type: Option<String>,
    rows: Vec<LeaderboardRow>,
}

/// The merged public leaderboard. Readable by anyone the API lets in (no admin) — the whole point is
/// that every operator benefits.
pub(crate) async fn get_leaderboard(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<LeaderboardParams>,
) -> Result<Json<LeaderboardResponse>, ApiError> {
    authenticate(&st, &headers).await?;
    let store = st.store.clone();
    let mut entries = spawn_db(move || store.list_collective_entries()).await?;

    // Retention, enforced at read time so the policy holds on every backend — including those whose
    // sweep is unimplemented, where the row survives on disk but is never published again.
    if let Some(cutoff) = st.collective.retention_cutoff(Utc::now()) {
        entries.retain(|e| e.received_at >= cutoff);
    }

    let mut rows = merge_leaderboard(&entries, st.collective.display_floor);

    // k-anonymity over SOURCES, applied before any filter: a row backed by fewer than
    // `min_contributors` distinct instances is not "the collective", it is that instance's private
    // eval results on a billboard — and a `?provider=` filter must never be able to strip a row down
    // to a lone source. (`min_cases` is a *case*-count floor within one contributor's bucket; it does
    // not anonymize across contributors. A 5000-case single-source row is still one source.)
    let k = st.collective.min_contributors;
    let held_back = {
        let before = rows.len();
        rows.retain(|r| r.n_contributors >= k);
        before - rows.len()
    };

    if let Some(tt) = q.task_type.as_deref() {
        rows.retain(|r| r.task_type == tt);
    }
    if let Some(p) = q.provider.as_deref() {
        rows.retain(|r| r.provider == p);
    }
    if let Some(j) = q.judge.as_deref() {
        rows.retain(|r| r.judge_providers.iter().any(|p| p == j));
    }
    // Rigor filters — deliberately applied HERE, after the `min_contributors` retain above, for the
    // same reason `?provider=` is: rigor is a low-cardinality but real fingerprint, and a filter that
    // ran before the source floor could strip a row down to a lone contributor's private eval.
    if let Some(d) = q.determinism.as_deref() {
        let want = lighttrack_core::canon_determinism(d);
        rows.retain(|r| want.is_some() && r.rigor.determinism == want);
    }
    if let Some(want) = q.frozen_dataset {
        rows.retain(|r| (r.rigor.frozen_dataset == lighttrack_core::Coverage::All) == want);
    }
    if let Some(want) = q.significance_tested {
        rows.retain(|r| (r.rigor.significance_tested == lighttrack_core::Coverage::All) == want);
    }

    // Header counts are computed over the FILTERED rows so they never disagree with what's shown.
    // Contributors backing the visible rows = distinct contributor ids of every stored entry whose
    // `(provider, model, task_type)` survived filtering (an entry's identity is normalized at ingest,
    // so its key matches the merged row's key exactly).
    let surviving: std::collections::BTreeSet<(&str, &str, &str)> = rows
        .iter()
        .map(|r| (r.provider.as_str(), r.model.as_str(), r.task_type.as_str()))
        .collect();
    let contributors = entries
        .iter()
        .filter(|e| {
            surviving.contains(&(e.provider.as_str(), e.model.as_str(), e.task_type.as_str()))
        })
        .map(|e| e.contributor_id.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let n_models = rows
        .iter()
        .map(|r| (r.provider.as_str(), r.model.as_str()))
        .collect::<std::collections::BTreeSet<_>>()
        .len();

    Ok(Json(LeaderboardResponse {
        contributors,
        n_models,
        n_rows: rows.len(),
        held_back,
        task_type: q.task_type,
        rows,
    }))
}
