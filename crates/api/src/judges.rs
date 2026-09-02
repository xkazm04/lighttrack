//! `GET /v1/judges/trust`, and the trust lookup every gate makes (M11).
//!
//! Before this module, "is this judge calibrated for this rubric?" had no answer a gate could ask
//! for: κ reached stdout and exit code 5 on whoever ran `lt-runner calibrate`, so
//! [`decide_gate`](crate::benchmarks::decide_gate) and
//! [`gate_promotion`](crate::prompts_gate::gate_promotion) both promoted without ever consulting it.
//! That is the uncalibrated gate — a green badge that means "the judge said so" and nothing about
//! whether the judge is worth believing.
//!
//! The verdict is three-valued and `unknown` is not `untrusted`: a judge nobody has measured has
//! taken no check, not failed one. A project that wants the absence to block says so explicitly
//! with [`Project::require_trusted_judge`].

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;

use lighttrack_core::{JudgeTrust, JudgeTrustVerdict, Project};

use crate::error::ApiError;
use crate::guards::{authenticate, resolve_read_project, NO_PROJECT_MSG};
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct TrustParams {
    project: Option<String>,
    /// The rubric being gated on. Omitted = the freeform (rubric-less) judge, which is a different
    /// question and never answers for a rubric.
    rubric_id: Option<String>,
    judge: String,
}

/// The trust of one `(project, rubric, judge)` triple, with the record that decided it.
pub(crate) async fn judge_trust(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<TrustParams>,
) -> Result<Json<JudgeTrustVerdict>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?
        .ok_or_else(|| ApiError::bad_request(NO_PROJECT_MSG))?;
    if q.judge.trim().is_empty() {
        return Err(ApiError::bad_request("judge is required"));
    }
    Ok(Json(
        lookup(&st, &project, q.rubric_id.as_deref(), &q.judge).await?,
    ))
}

/// The lookup a gate makes. Errors propagate rather than degrading to `unknown`: a backend that
/// cannot answer must produce a 501, because silently reporting "nobody has measured this" would
/// turn a capability gap into a permanent policy verdict.
pub(crate) async fn lookup(
    st: &AppState,
    project: &str,
    rubric_id: Option<&str>,
    judge: &str,
) -> Result<JudgeTrustVerdict, ApiError> {
    let store = st.store.clone();
    let (project, rubric_id, judge) = (
        project.to_string(),
        rubric_id.map(str::to_string),
        judge.to_string(),
    );
    let rec =
        spawn_db(move || store.latest_calibration(&project, rubric_id.as_deref(), &judge)).await?;
    Ok(JudgeTrustVerdict::from_record(rec))
}

/// The project row a gate needs to read its policy off. `None` when there is no such project — a
/// deployment can gate a benchmark whose project row was never created, and that is not a policy
/// opt-in.
pub(crate) async fn load_project(st: &AppState, pid: &str) -> Result<Option<Project>, ApiError> {
    let store = st.store.clone();
    let id = pid.to_string();
    spawn_db(move || store.get_project(&id)).await
}

/// Whether this project's policy refuses to promote on `trust`, and the message it refuses with.
///
/// Kept as a pure function of `(policy, trust)` so the rule is testable without a router, and so
/// both gates refuse with the same words — a gate that blocks with a different explanation than its
/// sibling is a gate people learn to ignore.
pub(crate) fn policy_block(project: Option<&Project>, v: &JudgeTrustVerdict) -> Option<String> {
    let require = project.map(|p| p.require_trusted_judge).unwrap_or(false);
    if !require || !v.trust.blocks_under_policy() {
        return None;
    }
    Some(match (v.trust, v.calibration.as_ref()) {
        (JudgeTrust::Untrusted, Some(c)) => format!(
            "the project requires a trusted judge, and '{}' is not trusted for this rubric: \
             κ {:.3} < {:.3} over n={} (measured {})",
            c.judge,
            c.kappa,
            c.kappa_bar,
            c.n,
            c.created_at.format("%Y-%m-%d")
        ),
        _ => "the project requires a trusted judge, and this judge has never been calibrated for \
              this rubric — run `lt-runner calibrate` against a labelled set first, or clear \
              `require_trusted_judge` on the project"
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use lighttrack_core::{CalibrationRecord, Redaction};

    fn project(require: bool) -> Project {
        Project {
            id: "p".into(),
            name: "p".into(),
            enabled: true,
            redaction: Redaction::default(),
            collective_opt_in: false,
            require_trusted_judge: require,
            created_at: Utc::now(),
            archived_at: None,
        }
    }

    fn untrusted() -> JudgeTrustVerdict {
        JudgeTrustVerdict::from_record(Some(CalibrationRecord {
            id: "c".into(),
            project_id: "p".into(),
            judge: "anthropic/haiku".into(),
            rubric_id: Some("rb".into()),
            dataset_id: None,
            dataset_version: None,
            kappa: 0.1,
            pearson: 0.2,
            mae: 0.3,
            rmse: 0.4,
            n: 12,
            kappa_bar: 0.6,
            trusted: false,
            created_at: Utc::now(),
        }))
    }

    /// A project that has not opted in is never blocked — the flag is off by default precisely so
    /// an upgrade does not start refusing promotions that were passing yesterday.
    #[test]
    fn the_policy_is_off_until_a_project_turns_it_on() {
        assert!(policy_block(None, &untrusted()).is_none());
        assert!(policy_block(Some(&project(false)), &untrusted()).is_none());
        assert!(policy_block(Some(&project(false)), &JudgeTrustVerdict::unknown()).is_none());
    }

    /// Both non-trusted answers block, and each says which one it was: "never calibrated" and
    /// "calibrated and failed" need different fixes.
    #[test]
    fn both_untrusted_and_unknown_block_but_explain_themselves_differently() {
        let p = project(true);
        let failed = policy_block(Some(&p), &untrusted()).expect("untrusted blocks");
        assert!(failed.contains("not trusted"), "{failed}");
        assert!(failed.contains("n=12"), "{failed}");

        let never = policy_block(Some(&p), &JudgeTrustVerdict::unknown()).expect("unknown blocks");
        assert!(never.contains("never been calibrated"), "{never}");
        assert!(never.contains("require_trusted_judge"), "{never}");
    }

    #[test]
    fn a_trusted_judge_is_never_blocked() {
        let mut v = untrusted();
        if let Some(c) = v.calibration.as_mut() {
            c.trusted = true;
            c.kappa = 0.9;
        }
        let v = JudgeTrustVerdict::from_record(v.calibration);
        assert_eq!(v.trust, JudgeTrust::Trusted);
        assert!(policy_block(Some(&project(true)), &v).is_none());
    }
}
