//! `GET /v1/collective/leaderboard` — the merged public leaderboard across all contributors.
//!
//! Assembly order is load-bearing and is the reason this lives on its own: read → retention →
//! merge → **k-anonymity over sources** → user filters → counts over what survived. A filter that
//! ran before the source floor could strip a row down to one contributor's private eval results.

use std::collections::BTreeSet;

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::collective::day_stamp;
use lighttrack_core::{merge_leaderboard, CollectiveEntry, LeaderboardRow};
use lighttrack_store::CollectiveFilter;

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
    /// Commensurability filter — keep only rows at least one of whose sources scored under this rubric
    /// fingerprint. An **input only**: the response publishes the rubric *count*, never a fingerprint.
    /// A caller supplies a fingerprint it already holds (its own digest's, typically) to ask for a
    /// board it can compare itself against.
    rubric: Option<String>,
    /// Freshness filter — keep only rows with at least one source received within this many days. A
    /// row that cannot prove its age is dropped rather than assumed current.
    fresh_days: Option<u32>,
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
    /// The board's own date: the newest evidence behind any **visible** row (day-coarsened,
    /// fixed-width `RFC3339(Nanos, Z)`). An unfed board reads as what it is instead of as current.
    /// `None` when nothing is visible — an empty board owns no date.
    #[serde(skip_serializing_if = "Option::is_none")]
    as_of: Option<String>,
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
    // Retention is enforced at read time so the policy holds on every backend — including those
    // whose sweep is unimplemented, where the row survives on disk but is never published again.
    // It is also the ONLY predicate pushed into the store: it drops rows that must not be published
    // at all, so applying it early cannot strip a merged row down to a lone contributor. Every user
    // filter below runs after the merge and after the source floor, for exactly that reason.
    let filter = CollectiveFilter {
        received_after: st.collective.retention_cutoff(Utc::now()),
    };
    let store = st.store.clone();
    let entries = spawn_db(move || store.list_collective_entries_filtered(&filter)).await?;

    let rows = merge_leaderboard(&entries, st.collective.display_floor);
    let n = narrow(
        rows,
        &entries,
        &q,
        st.collective.min_contributors,
        Utc::now(),
    );

    Ok(Json(LeaderboardResponse {
        contributors: n.contributors,
        n_models: n.n_models,
        n_rows: n.rows.len(),
        held_back: n.held_back,
        as_of: n.as_of,
        task_type: q.task_type,
        rows: n.rows,
    }))
}

/// The filtered board plus the counts computed over it.
struct Narrowed {
    rows: Vec<LeaderboardRow>,
    held_back: usize,
    contributors: usize,
    n_models: usize,
    as_of: Option<String>,
}

/// Everything after the merge: the source floor, then the user filters, then the counts over what
/// survived. A pure function of `(rows, entries, params, k, now)` so the **order** — which is the
/// privacy property, not a detail — is testable without an HTTP stack.
fn narrow(
    mut rows: Vec<LeaderboardRow>,
    entries: &[CollectiveEntry],
    q: &LeaderboardParams,
    k: u32,
    now: DateTime<Utc>,
) -> Narrowed {
    // k-anonymity over SOURCES, applied before any filter: a row backed by fewer than
    // `min_contributors` distinct instances is not "the collective", it is that instance's private
    // eval results on a billboard — and a `?provider=` filter must never be able to strip a row down
    // to a lone source. (`min_cases` is a *case*-count floor within one contributor's bucket; it does
    // not anonymize across contributors. A 5000-case single-source row is still one source.)
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
    // Commensurability filter, matched against the *stored entries* because the published row carries
    // only the rubric COUNT: a fingerprint is a contributor-chosen high-cardinality hash, so it is an
    // input here and never an output (see `lighttrack_core::collective::provenance`). A source tagged
    // `mixed` already disagreed with itself and cannot vouch for one rubric, so it does not match.
    if let Some(want) = q.rubric.as_deref() {
        let keyed: BTreeSet<(&str, &str, &str)> = entries
            .iter()
            .filter(|e| e.rubric_fingerprint.as_deref() == Some(want))
            .map(|e| (e.provider.as_str(), e.model.as_str(), e.task_type.as_str()))
            .collect();
        rows.retain(|r| {
            keyed.contains(&(r.provider.as_str(), r.model.as_str(), r.task_type.as_str()))
        });
    }
    // Freshness filter, same position and the same reason.
    if let Some(days) = q.fresh_days {
        let cutoff = day_stamp(now - Duration::days(i64::from(days.min(36_500))));
        rows.retain(|r| r.newest_at.as_deref().is_some_and(|t| t >= cutoff.as_str()));
    }

    // Header counts are computed over the FILTERED rows so they never disagree with what's shown.
    // Contributors backing the visible rows = distinct contributor ids of every stored entry whose
    // `(provider, model, task_type)` survived filtering (an entry's identity is normalized at ingest,
    // so its key matches the merged row's key exactly).
    let surviving: BTreeSet<(&str, &str, &str)> = rows
        .iter()
        .map(|r| (r.provider.as_str(), r.model.as_str(), r.task_type.as_str()))
        .collect();
    let contributors = entries
        .iter()
        .filter(|e| {
            surviving.contains(&(e.provider.as_str(), e.model.as_str(), e.task_type.as_str()))
        })
        .map(|e| e.contributor_id.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let n_models = rows
        .iter()
        .map(|r| (r.provider.as_str(), r.model.as_str()))
        .collect::<BTreeSet<_>>()
        .len();
    let as_of = rows.iter().filter_map(|r| r.newest_at.clone()).max();

    Narrowed {
        rows,
        held_back,
        contributors,
        n_models,
        as_of,
    }
}

#[cfg(test)]
mod tests;
