//! Scores (Phase 3) — the runner posts judge verdicts here; clients read them back.
//!
//! `GET /v1/scores` returns each score's structured `detail` (per-dimension breakdown, agreement,
//! sample accounting, bias/injection flags) when the backend stored one. Additive and nullable, so
//! clients that ignore the field are unaffected.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;

use lighttrack_core::Score;

use crate::error::ApiError;
use crate::guards::{authenticate, resolve_ingest_project, resolve_read_project};
use crate::state::{spawn_db, AppState};

pub(crate) async fn post_score(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(mut s): Json<Score>,
) -> Result<Json<Score>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    s.project_id = resolve_ingest_project(&p, &s.project_id)?;
    // Bound verdict provenance at the boundary, not at the (many) callers: a score row is hot, and a
    // client posting unbounded reasoning must not be able to balloon it. An empty detail is dropped
    // so `{}` never persists as if it were provenance.
    s.detail = s.detail.take().map(lighttrack_core::ScoreDetail::capped).filter(|d| !d.is_empty());
    let store = st.store.clone();
    let s2 = s.clone();
    spawn_db(move || store.insert_score(&s2)).await?;
    // Best-effort quality-regression detection over the rolling per-(project,rubric) score window.
    st.alerts.record_score(&s);
    Ok(Json(s))
}

#[derive(Deserialize)]
pub(crate) struct ScoresParams {
    project: Option<String>,
    limit: Option<usize>,
}

pub(crate) async fn get_scores(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ScoresParams>,
) -> Result<Json<Vec<Score>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let store = st.store.clone();
    let limit = q.limit.unwrap_or(50).min(1000);
    let scores = spawn_db(move || store.list_scores(project.as_deref(), limit)).await?;
    Ok(Json(scores))
}
