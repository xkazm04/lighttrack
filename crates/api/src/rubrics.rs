//! Rubrics (Phase 3.6c) — structured, multi-dimension judging criteria.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use lighttrack_core::{new_id, CalibrationRecord, Rubric, RubricDimension};

use crate::auth::Principal;
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin, resolve_read_project};
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct CreateRubricReq {
    name: String,
    dimensions: Vec<RubricDimension>,
    #[serde(default = "default_rubric_threshold")]
    threshold: f64,
}

fn default_rubric_threshold() -> f64 {
    0.7
}

pub(crate) async fn create_rubric(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<CreateRubricReq>,
) -> Result<Json<Rubric>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let r = Rubric {
        id: new_id(),
        project_id: pid,
        name: req.name,
        dimensions: req.dimensions,
        threshold: req.threshold,
        // Every rubric starts at generation 1; POST /v1/rubrics/:id/versions mints the next.
        version: 1,
        supersedes: None,
        created_at: Utc::now(),
    };
    let store = st.store.clone();
    let r2 = r.clone();
    spawn_db(move || store.create_rubric(&r2)).await?;
    Ok(Json(r))
}

pub(crate) async fn list_rubrics(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
) -> Result<Json<Vec<Rubric>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    resolve_read_project(&p, Some(&pid))?;
    let store = st.store.clone();
    let v = spawn_db(move || store.list_rubrics(&pid)).await?;
    Ok(Json(v))
}

/// A rubric, plus whether anything has ever been measured against it (M11).
///
/// `active` is not a switch anybody flips: it is `trust != unknown` — "at least one judge has been
/// calibrated for this rubric". It matters because a **new version is a new id**, so a version
/// inherits none of its predecessor's calibration: promoting to it silently swaps a measured
/// instrument for an unmeasured one, and until now nothing said so.
#[derive(Serialize)]
pub(crate) struct RubricView {
    #[serde(flatten)]
    rubric: Rubric,
    /// At least one calibration exists for this rubric id.
    active: bool,
    /// The judges measured against it, newest first — so "calibrate this version" is an actionable
    /// instruction rather than a flag to hunt for.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    calibrated_judges: Vec<String>,
}

pub(crate) async fn get_rubric(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<RubricView>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let store = st.store.clone();
    let id2 = id.clone();
    let r = spawn_db(move || store.get_rubric(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("rubric '{id}' not found")))?;
    if let Principal::Project {
        project_id: pid, ..
    } = &p
    {
        if &r.project_id != pid {
            return Err(ApiError::forbidden("key not authorized for that rubric"));
        }
    }
    let store = st.store.clone();
    let project = r.project_id.clone();
    let history = spawn_db(move || store.list_calibrations(Some(&project), 0, None)).await?;
    let calibrated_judges = judges_for(&history, &r.id);
    Ok(Json(RubricView {
        active: !calibrated_judges.is_empty(),
        calibrated_judges,
        rubric: r,
    }))
}

/// Distinct judges calibrated for `rubric_id`, newest-first and deduplicated. Exact on the rubric
/// id, so a sibling rubric's — or a freeform — calibration never makes this one look measured.
fn judges_for(history: &[CalibrationRecord], rubric_id: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in history {
        if c.rubric_id.as_deref() == Some(rubric_id) && !out.contains(&c.judge) {
            out.push(c.judge.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(rubric: Option<&str>, judge: &str) -> CalibrationRecord {
        CalibrationRecord {
            id: new_id(),
            project_id: "p".into(),
            judge: judge.into(),
            rubric_id: rubric.map(str::to_string),
            dataset_id: None,
            dataset_version: None,
            kappa: 0.8,
            pearson: 0.9,
            mae: 0.1,
            rmse: 0.1,
            n: 20,
            kappa_bar: 0.6,
            trusted: true,
            created_at: Utc::now(),
        }
    }

    /// The whole point of `active`: a new rubric version is a new id and starts unmeasured, however
    /// well-calibrated the version it superseded was.
    #[test]
    fn a_rubric_is_active_only_on_its_own_calibrations() {
        let history = vec![
            rec(Some("v1"), "anthropic/haiku"),
            rec(Some("v1"), "openai/gpt"),
            rec(Some("v1"), "anthropic/haiku"),
            rec(None, "anthropic/haiku"),
        ];
        assert_eq!(
            judges_for(&history, "v1"),
            vec!["anthropic/haiku".to_string(), "openai/gpt".into()],
            "each judge once, newest first"
        );
        assert!(
            judges_for(&history, "v2").is_empty(),
            "a new version inherits nothing — not from its predecessor, not from the freeform \
             calibration"
        );
    }
}

/// Body for `POST /v1/rubrics/:id/versions` — a copy-with-changes of an existing rubric.
///
/// Both fields are optional: a version that only re-weights the dimensions should not have to
/// restate the threshold, and one that only moves the threshold should not have to restate the
/// dimensions. Whatever is omitted is carried forward from the rubric being superseded.
#[derive(Deserialize)]
pub(crate) struct NewVersionReq {
    dimensions: Option<Vec<RubricDimension>>,
    threshold: Option<f64>,
}

/// `POST /v1/rubrics/:id/versions` — the next generation of a rubric.
///
/// A **new row with a new id**, linked back to the old one, never a mutation. Verdicts already
/// stored cite the old rubric's id, and editing that row underneath them would silently change what
/// those verdicts claim to have measured — a restatement of history exactly like the one the revenue
/// upsert refuses. The old rubric stays readable and stays cited.
///
/// Deliberately **no** `active` flag here: promoting a version behind a calibration gate is M11's
/// job, and a flag shipped now would be an ungated promotion switch wearing the same name.
pub(crate) async fn create_rubric_version(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<NewVersionReq>,
) -> Result<Json<Rubric>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let id2 = id.clone();
    let prev = spawn_db(move || store.get_rubric(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("rubric '{id}' not found")))?;

    let dimensions = req.dimensions.unwrap_or_else(|| prev.dimensions.clone());
    if dimensions.is_empty() {
        return Err(ApiError::bad_request(
            "a rubric version needs at least one dimension",
        ));
    }
    let next = prev.next_version(dimensions, req.threshold.unwrap_or(prev.threshold));

    let store = st.store.clone();
    let to_insert = next.clone();
    spawn_db(move || store.create_rubric(&to_insert)).await?;
    Ok(Json(next))
}
