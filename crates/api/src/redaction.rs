//! `GET /v1/projects/:id/redaction` — what this project's stored rows actually had done to them.
//!
//! The scrub is configured server-globally and the persistence policy per project, so an operator
//! could read both settings and still not know what is *in the database*: settings change, upgrades
//! change defaults, and rows written under an older posture keep sitting there. This route answers
//! from the rows rather than from the configuration.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_store::RedactionPostureRow;

use crate::error::ApiError;
use crate::guards::{authenticate, resolve_read_project};
use crate::state::{spawn_db, AppState};

#[derive(Debug, Deserialize)]
pub(crate) struct PostureParams {
    /// RFC3339 inclusive lower bound on **arrival** time. Defaults to 30 days back.
    since: Option<String>,
}

/// The posture report as served.
#[derive(Debug, Serialize)]
pub(crate) struct PostureBody {
    project_id: String,
    since: String,
    /// The rule set this instance would scrub with *right now*. A group whose `stamp.rules` differs
    /// from it is a cohort scrubbed by a previous generation of the rules — the thing you need to
    /// know before comparing two rows' redaction counts, and the reason a bare count was not enough.
    current_rules: &'static str,
    /// Events in the window carrying no stamp at all: written before the stamp existed, or by a
    /// path that does not scrub. Reported as its own number, never folded into "not scrubbed" —
    /// "we do not know" is the finding an operator has to act on.
    unaccounted_events: u64,
    total_events: u64,
    groups: Vec<RedactionPostureRow>,
}

/// `GET /v1/projects/:id/redaction` — admin, or the project's own read key.
pub(crate) async fn get_redaction_posture(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Query(q): Query<PostureParams>,
) -> Result<Json<PostureBody>, ApiError> {
    let principal = authenticate(&st, &headers).await?;
    // A project key may read only its own posture; an admin key asking for a project gets that
    // project. Same resolution as every other project-scoped read.
    resolve_read_project(&principal, Some(&pid))?;

    let since = match q.since.as_deref() {
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .map_err(|_| ApiError::bad_request(format!("invalid RFC3339 timestamp: {s}")))?,
        None => Utc::now() - Duration::days(30),
    };

    let store = st.store.clone();
    let target = pid.clone();
    let groups = spawn_db(move || store.redaction_posture(Some(&target), since)).await?;

    let total_events = groups.iter().map(|g| g.events).sum();
    let unaccounted_events = groups
        .iter()
        .filter(|g| g.stamp.is_none())
        .map(|g| g.events)
        .sum();
    Ok(Json(PostureBody {
        project_id: pid,
        since: since.to_rfc3339(),
        current_rules: lighttrack_anon::rules_fingerprint(),
        unaccounted_events,
        total_events,
        groups,
    }))
}
