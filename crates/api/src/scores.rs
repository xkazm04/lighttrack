//! Scores (Phase 3) — the runner posts judge verdicts here; clients read them back.
//!
//! `GET /v1/scores` returns each score's structured `detail` (per-dimension breakdown, agreement,
//! sample accounting, bias/injection flags) when the backend stored one. Additive and nullable, so
//! clients that ignore the field are unaffected.
//!
//! `GET /v1/scores?run=<benchmark_run_id>` narrows to one benchmark run's case results, in case
//! order — every mode stamps `run_id`/`case_index` on the verdicts it posts. A backend that hasn't
//! ported run scoping answers 501 rather than an empty list that would read as "no failures".

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;

use std::collections::HashMap;

use lighttrack_core::{Label, LabelFilter, Score, ScoreKind, DEFAULT_RUBRIC_THRESHOLD};

use crate::error::ApiError;
use crate::guards::{authenticate, resolve_ingest_project, resolve_read_project};
use lighttrack_store::ScoreFilter;

use crate::scores_review::review_reasons;
use crate::state::{spawn_db, AppState};

pub(crate) async fn post_score(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(mut s): Json<Score>,
) -> Result<Json<Score>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    s.project_id = resolve_ingest_project(&p, &s.project_id)?;
    validate_verdict(&s)?;
    // Bound verdict provenance at the boundary, not at the (many) callers: a score row is hot, and a
    // client posting unbounded reasoning must not be able to balloon it. An empty detail is dropped
    // so `{}` never persists as if it were provenance.
    s.detail = s
        .detail
        .take()
        .map(lighttrack_core::ScoreDetail::capped)
        .filter(|d| !d.is_empty());
    let store = st.store.clone();
    let s2 = s.clone();
    spawn_db(move || store.insert_score(&s2)).await?;
    // Best-effort quality-regression detection over the rolling per-(project,rubric) score window.
    st.alerts.record_score(&s);
    Ok(Json(s))
}

/// The numbers a verdict must satisfy before it is stored. Nothing checked them: a NaN `value`
/// reached the alert window and every mean it is averaged into (Postgres stores it and AVG turns
/// NaN; SQLite stores NULL and the row silently drops out of the denominator), a `max` of 0 made
/// every downstream ratio a division by zero, and a `value` above `max` inflated pass rates. A
/// verdict with no judge or no label is not a verdict either.
fn validate_verdict(s: &Score) -> Result<(), ApiError> {
    let bad = |m: String| Err(ApiError::bad_request(m));
    if !(s.max.is_finite() && s.max > 0.0) {
        return bad(format!(
            "max must be a positive finite number (got {})",
            s.max
        ));
    }
    if !s.value.is_finite() || s.value < 0.0 || s.value > s.max {
        return bad(format!(
            "value must be within 0..={} (got {})",
            s.max, s.value
        ));
    }
    if let Some(c) = s.cost_usd {
        if !(c.is_finite() && c >= 0.0) {
            return bad(format!(
                "cost_usd must be a non-negative finite number (got {c})"
            ));
        }
    }
    if s.rubric.trim().is_empty() {
        return bad("rubric (the verdict's label) must not be blank".into());
    }
    if s.scored_by.trim().is_empty() {
        return bad("scored_by (the judge) must not be blank".into());
    }
    Ok(())
}

#[derive(Deserialize)]
pub(crate) struct ScoresParams {
    project: Option<String>,
    limit: Option<usize>,
    /// Return only the case results of this benchmark run, in case order — the answer to "why did
    /// run 47 fail?". Every mode (simple, rubric, compare, pairwise) stamps its run id on the
    /// verdicts it posts, so this works for all of them, not just the ones that inline case JSON in
    /// the run report.
    run: Option<String>,
    /// Only verdicts judged against this rubric. The join the free-text `rubric` label could
    /// never be: it survives a rename and separates two rubrics that share a name.
    rubric_id: Option<String>,
    /// Only verdicts of this kind (`freeform` | `rubric` | `bench_case` | `compare_cell` |
    /// `pairwise_game` | `calibration` | `trace`).
    kind: Option<String>,
    /// Only verdicts a human should look at: the judge disagreed with itself or with the person who
    /// graded the same subject, flagged an injection, hit a dimension floor, or landed within a
    /// hair of the pass threshold (M11). Accepts `1`/`true`.
    needs_review: Option<String>,
    /// The pass threshold `needs_review` measures "near" against. Defaults to 0.7, the same default
    /// a rubric takes.
    threshold: Option<f64>,
}

/// `needs_review=1` / `=true`. A value we do not recognise is a 400 rather than a silent "no":
/// a triage question answered "nothing to review" because of a typo is the worst possible answer.
fn parse_flag(v: &str) -> Result<bool, ApiError> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        other => Err(ApiError::bad_request(format!(
            "invalid 'needs_review' {other:?}: expected 1 | 0 | true | false"
        ))),
    }
}

pub(crate) async fn get_scores(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ScoresParams>,
) -> Result<Json<Vec<Score>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let project_for_labels = project.clone();
    let store = st.store.clone();
    // A run's cases are one dataset pass, so the run-scoped read allows a bigger page than the
    // "latest N scores" firehose — a 500-case benchmark should come back in one request.
    let scores = match q.run {
        Some(run) => {
            let limit = q.limit.unwrap_or(5000).min(10_000);
            // The project scope goes into the query, not a post-filter: a project key must not be
            // able to read another project's run by guessing its id.
            spawn_db(move || store.list_run_scores(&run, project.as_deref().into(), limit)).await?
        }
        None => {
            let limit = q.limit.unwrap_or(50).min(1000);
            let filter = ScoreFilter {
                rubric_id: q.rubric_id.clone(),
                kind: q.kind.clone(),
            };
            if let Some(k) = filter.kind.as_deref() {
                // Reject an unknown kind rather than answering with an empty page: 'no bench
                // cases' and 'you spelled the kind wrong' must not look identical. The accepted
                // set is derived from the enum, so adding a variant cannot leave this stale.
                if ScoreKind::parse(k).is_none() {
                    let expected = ScoreKind::ALL
                        .iter()
                        .map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" | ");
                    return Err(ApiError::bad_request(format!(
                        "invalid 'kind' {k:?}: expected {expected}"
                    )));
                }
            }
            // The unfiltered listing stays on `list_scores`: it is every dashboard's hot path,
            // and a backend that has not ported the filters must still serve it.
            if filter.is_empty() {
                spawn_db(move || store.list_scores(project.as_deref().into(), limit)).await?
            } else {
                spawn_db(move || {
                    store.list_scores_filtered(project.as_deref().into(), &filter, limit)
                })
                .await?
            }
        }
    };
    let needs_review = match q.needs_review.as_deref() {
        Some(v) => parse_flag(v)?,
        None => false,
    };
    if !needs_review {
        return Ok(Json(scores));
    }
    Ok(Json(
        triage(&st, project_for_labels, scores, q.threshold).await?,
    ))
}

/// Narrow a page of verdicts to the ones worth a person's time.
///
/// The labels are fetched **once** for the whole page and joined in memory, rather than one lookup
/// per score: a 500-case run would otherwise be 500 round trips to answer one triage question. The
/// join is on both the score's own id and the event it judged, because a human may have graded
/// either — and a grade on the event is as much a contradiction of the verdict as a grade on the
/// verdict itself.
///
/// A store that cannot serve labels errors (501) rather than quietly answering with the
/// detail-only half: "the judge disagreed with a human" is half of what this question means, and a
/// page missing it would read as a complete answer.
async fn triage(
    st: &AppState,
    project: Option<String>,
    scores: Vec<Score>,
    threshold: Option<f64>,
) -> Result<Vec<Score>, ApiError> {
    let filter = LabelFilter {
        project,
        limit: LabelFilter::MAX_LIMIT,
        ..Default::default()
    };
    let store = st.store.clone();
    let labels = spawn_db(move || store.list_labels(&filter)).await?;
    let mut by_subject: HashMap<&str, &Label> = HashMap::new();
    for l in &labels {
        // Newest-first from the store, so the first label seen for a subject is the current one.
        by_subject.entry(l.subject.id()).or_insert(l);
    }
    let threshold = threshold.unwrap_or(DEFAULT_THRESHOLD);
    Ok(scores
        .into_iter()
        .filter(|s| {
            let label = by_subject
                .get(s.id.as_str())
                .or_else(|| s.event_id.as_deref().and_then(|e| by_subject.get(e)))
                .copied();
            !review_reasons(s, threshold, label).is_empty()
        })
        .collect())
}

/// The same default `POST /v1/projects/:id/rubrics` gives a rubric with no threshold of its own —
/// read from core rather than restated, so the two cannot drift.
const DEFAULT_THRESHOLD: f64 = DEFAULT_RUBRIC_THRESHOLD;

#[cfg(test)]
mod tests {
    use super::validate_verdict;
    use lighttrack_core::{Score, ScoreKind};

    fn score(value: f64, max: f64) -> Score {
        Score {
            id: "s".into(),
            project_id: "p".into(),
            event_id: None,
            rubric: "quality".into(),
            rubric_id: None,
            kind: ScoreKind::Freeform,
            value,
            max,
            pass: None,
            reasoning: None,
            detail: None,
            run_id: None,
            case_index: None,
            scored_by: "judge".into(),
            cost_usd: None,
            created_at: chrono::Utc::now(),
        }
    }

    /// The shapes that used to reach the alert window and the averages.
    #[test]
    fn a_verdict_that_is_not_a_number_in_range_is_refused() {
        assert!(validate_verdict(&score(0.8, 1.0)).is_ok());
        assert!(
            validate_verdict(&score(5.0, 5.0)).is_ok(),
            "value == max is a full score"
        );
        assert!(validate_verdict(&score(f64::NAN, 1.0)).is_err(), "NaN");
        assert!(validate_verdict(&score(f64::INFINITY, 1.0)).is_err(), "inf");
        assert!(validate_verdict(&score(1.5, 1.0)).is_err(), "above max");
        assert!(validate_verdict(&score(-0.1, 1.0)).is_err(), "negative");
        assert!(validate_verdict(&score(0.5, 0.0)).is_err(), "max 0");
        let mut s = score(0.5, 1.0);
        s.cost_usd = Some(-1.0);
        assert!(validate_verdict(&s).is_err(), "negative cost");
        let mut s = score(0.5, 1.0);
        s.scored_by = " ".into();
        assert!(validate_verdict(&s).is_err(), "no judge");
        let mut s = score(0.5, 1.0);
        s.rubric = "".into();
        assert!(validate_verdict(&s).is_err(), "no label");
    }
}
