//! The alert ledger's read/act surface: `GET /v1/alerts`, `POST /v1/alerts/:id/ack`,
//! `POST /v1/alerts/:id/resolution`.
//!
//! Before these routes there was no way to ask what had fired. An alert existed as a webhook post
//! and a `tracing::warn!`, so "what did LightTrack tell us last week, and did anyone act on it" was
//! a question about the receiver's inbox rather than about LightTrack. Acknowledgement and
//! resolution are separate facts on purpose: someone *saw* it, and something *came of it*.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use lighttrack_core::{AlertKind, Scope};
use lighttrack_store::AlertFilter;

use crate::auth::Principal;
use crate::auth_scopes::ensure_scope;
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin, resolve_read_project};
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct AlertsQuery {
    project: Option<String>,
    kind: Option<String>,
    since: Option<String>,
    acked: Option<bool>,
    limit: Option<usize>,
    cursor: Option<String>,
}

pub(crate) async fn list_alerts(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AlertsQuery>,
) -> Result<Json<Value>, ApiError> {
    let principal = authenticate(&st, &headers).await?;
    // Project scoping goes through the same guard every other read uses, so a project key cannot
    // read another tenant's alerts by naming it.
    let project = resolve_read_project(&principal, q.project.as_deref())?;
    let kind = match q.kind.as_deref() {
        Some(k) => Some(AlertKind::from_wire(k).ok_or_else(|| {
            ApiError::bad_request(format!(
                "unknown alert kind '{k}'. Known kinds: {}",
                AlertKind::ALL
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?),
        None => None,
    };
    let since = match q.since.as_deref() {
        Some(s) => Some(parse_since(s)?),
        None => None,
    };
    let filter = AlertFilter {
        project,
        kind,
        since,
        acked: q.acked,
        limit: q.limit.unwrap_or(0),
        cursor: q.cursor,
    };
    let store = st.store.clone();
    let rows = spawn_db(move || store.list_alerts(&filter)).await?;
    let next = rows.last().map(|a| {
        lighttrack_store::codec::encode_event_cursor(
            &lighttrack_store::codec::fmt_ts(a.fired_at),
            &a.id,
        )
    });
    Ok(Json(json!({ "alerts": rows, "next_cursor": next })))
}

#[derive(Deserialize)]
pub(crate) struct AckReq {
    /// Who acknowledged it. Free text — an on-call handle, an email, a runbook link.
    #[serde(default)]
    by: Option<String>,
}

pub(crate) async fn ack_alert(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Option<Json<AckReq>>,
) -> Result<Json<Value>, ApiError> {
    let principal = authenticate(&st, &headers).await?;
    ensure_scope(&principal, Scope::Manage)?;
    let by = body
        .and_then(|Json(b)| b.by)
        .unwrap_or_else(|| principal_label(&principal));
    let store = st.store.clone();
    let id2 = id.clone();
    let at = Utc::now();
    if !spawn_db(move || store.ack_alert(&id2, &by, at)).await? {
        return Err(ApiError::not_found(format!("alert '{id}' not found")));
    }
    Ok(Json(json!({ "acked": id, "acked_at": at })))
}

/// `POST /v1/alerts/:id/resolution` — what came of the alert. The responder posts its diagnosis
/// here, which is what turns a fired alert into a closed loop rather than a notification.
pub(crate) async fn attach_resolution(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(resolution): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let id2 = id.clone();
    let r = resolution.clone();
    if !spawn_db(move || store.attach_alert_resolution(&id2, &r)).await? {
        return Err(ApiError::not_found(format!("alert '{id}' not found")));
    }
    Ok(Json(json!({ "resolved": id })))
}

fn principal_label(p: &Principal) -> String {
    match p {
        Principal::Admin => "admin".into(),
        Principal::Dev => "dev".into(),
        Principal::Project { key_id, .. } => format!("key:{key_id}"),
    }
}

/// `since` accepts an RFC3339 instant or a relative `7d` / `24h` / `30m`, matching the other
/// windowed reads.
fn parse_since(s: &str) -> Result<DateTime<Utc>, ApiError> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num.parse().map_err(|_| {
        ApiError::bad_request(format!(
            "since '{s}' is neither an RFC3339 instant nor a relative window like 24h / 7d / 30m"
        ))
    })?;
    let d = match unit {
        "d" => chrono::Duration::days(n),
        "h" => chrono::Duration::hours(n),
        "m" => chrono::Duration::minutes(n),
        _ => {
            return Err(ApiError::bad_request(format!(
                "since '{s}' has an unknown unit '{unit}' (use d, h or m)"
            )))
        }
    };
    Ok(Utc::now() - d)
}
