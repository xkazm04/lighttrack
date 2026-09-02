//! `GET /v1/rollup` — the grouped cost/usage question, asked directly.
//!
//! Every cost surface (`/v1/costs`, `/v1/usecases`, `/v1/limits/usage`, `/v1/margin/*`,
//! `/v1/forecast`) is one fixed grouping of this. Exposing the primitive means a question nobody
//! anticipated ("cost by customer per day", "which API key drives the error rate") is a query rather
//! than a new endpoint.
//!
//! Two access rules. A project key sees only its own project (`resolve_read_project`, as everywhere
//! else). And the `api_key` dimension is **admin-only**: grouping by it enumerates a project's keys
//! by id, which a key embedded in a shipped client app has no business learning about its siblings.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::{Dimension, RollupQuery, RollupRow, TimeKey, MAX_GROUP_BY};

use crate::auth::Principal;
use crate::error::ApiError;
use crate::guards::{authenticate, resolve_read_project};
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct RollupParams {
    project: Option<String>,
    /// Comma-separated dimensions, 1..=3: `project,provider,model,name,api_key,customer,product,
    /// prompt,day`.
    by: Option<String>,
    /// RFC3339 window (`since` inclusive, `until` exclusive). `since` defaults to 30 days back.
    since: Option<String>,
    until: Option<String>,
    /// Which timestamp the window and the `day` bucket read: `ts` (default) or `received_at`.
    time: Option<String>,
    /// Repeatable-by-comma equality predicates, `dimension:value` (the value may contain `:`).
    filter: Option<String>,
}

/// The response echoes the grouping so a client can read `keys` positionally without having to
/// re-parse its own request — and so a cached payload stays self-describing.
#[derive(Serialize)]
pub(crate) struct RollupResponse {
    group_by: Vec<Dimension>,
    time_key: &'static str,
    rows: Vec<RollupRow>,
}

pub(crate) async fn get_rollup(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<RollupParams>,
) -> Result<Json<RollupResponse>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let admin = matches!(p, Principal::Admin | Principal::Dev);

    let group_by = parse_group_by(q.by.as_deref(), admin)?;
    let until = parse_opt_ts("until", q.until.as_deref())?;
    let since = parse_opt_ts("since", q.since.as_deref())?
        .unwrap_or_else(|| until.unwrap_or_else(Utc::now) - chrono::Duration::days(30));
    let time_key = match q.time.as_deref() {
        None => TimeKey::Ts,
        Some(s) => TimeKey::parse(s)
            .ok_or_else(|| ApiError::bad_request(format!("unknown time key '{s}'")))?,
    };
    let filter = parse_filters(q.filter.as_deref(), admin)?;
    if until.is_some_and(|u| u < since) {
        return Err(ApiError::bad_request("`until` precedes `since`"));
    }

    let store = st.store.clone();
    let group_for_query = group_by.clone();
    // The query borrows the project id, so it is built inside the blocking closure that owns it.
    let rows = spawn_db(move || {
        let mut query = RollupQuery::new(&group_for_query, since)
            .project(project.as_deref())
            .until(until)
            .time_key(time_key);
        query.filter = filter;
        store.rollup(&query)
    })
    .await?;

    Ok(Json(RollupResponse {
        group_by,
        time_key: time_key.as_str(),
        rows,
    }))
}

/// Parse `?by=a,b,c` into 1..=[`MAX_GROUP_BY`] dimensions. Defaults to `provider,model` — the
/// grouping a cost question is usually asking for.
fn parse_group_by(raw: Option<&str>, admin: bool) -> Result<Vec<Dimension>, ApiError> {
    let raw = raw.unwrap_or("provider,model");
    let mut out = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let d = Dimension::parse(part)
            .ok_or_else(|| ApiError::bad_request(format!("unknown dimension '{part}'")))?;
        gate_api_key(d, admin)?;
        if out.contains(&d) {
            return Err(ApiError::bad_request(format!(
                "duplicate dimension '{part}'"
            )));
        }
        out.push(d);
    }
    if out.is_empty() {
        return Err(ApiError::bad_request("`by` needs at least one dimension"));
    }
    if out.len() > MAX_GROUP_BY {
        return Err(ApiError::bad_request(format!(
            "`by` takes at most {MAX_GROUP_BY} dimensions"
        )));
    }
    Ok(out)
}

fn parse_filters(raw: Option<&str>, admin: bool) -> Result<Vec<(Dimension, String)>, ApiError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        // Split on the FIRST `:` so a value may contain one (model ids and prompt tags do).
        let (dim, value) = part.split_once(':').ok_or_else(|| {
            ApiError::bad_request(format!("filter '{part}' is not `dimension:value`"))
        })?;
        let d = Dimension::parse(dim)
            .ok_or_else(|| ApiError::bad_request(format!("unknown dimension '{dim}'")))?;
        gate_api_key(d, admin)?;
        if d == Dimension::Day {
            return Err(ApiError::bad_request(
                "the `day` dimension cannot be filtered on; use `since`/`until`",
            ));
        }
        out.push((d, value.to_string()));
    }
    Ok(out)
}

/// Grouping or filtering by `api_key` enumerates a project's key ids. That is an operator question,
/// not a question the key shipped inside a client app gets to ask about its siblings.
fn gate_api_key(d: Dimension, admin: bool) -> Result<(), ApiError> {
    if d == Dimension::ApiKey && !admin {
        return Err(ApiError::forbidden("the `api_key` dimension is admin-only"));
    }
    Ok(())
}

fn parse_opt_ts(field: &str, raw: Option<&str>) -> Result<Option<DateTime<Utc>>, ApiError> {
    match raw {
        Some(s) => Ok(Some(
            DateTime::parse_from_rfc3339(s)
                .map_err(|e| ApiError::bad_request(format!("invalid '{field}' timestamp: {e}")))?
                .with_timezone(&Utc),
        )),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_grouping_is_provider_model() {
        assert_eq!(
            parse_group_by(None, false).ok().expect("default"),
            vec![Dimension::Provider, Dimension::Model]
        );
    }

    #[test]
    fn an_unknown_or_duplicate_or_oversized_grouping_is_a_400() {
        assert!(parse_group_by(Some("nonsense"), true).is_err());
        assert!(parse_group_by(Some("model,model"), true).is_err());
        assert!(parse_group_by(Some("model,provider,name,day"), true).is_err());
        assert!(parse_group_by(Some(","), true).is_err(), "empty list");
    }

    /// The one dimension that leaks something: a project key must not be able to enumerate the
    /// project's other keys by grouping on them.
    #[test]
    fn the_api_key_dimension_is_admin_only_in_both_by_and_filter() {
        assert!(parse_group_by(Some("api_key"), false).is_err());
        assert!(parse_group_by(Some("api_key"), true).is_ok());
        assert!(parse_filters(Some("api_key:k-1"), false).is_err());
        assert!(parse_filters(Some("api_key:k-1"), true).is_ok());
    }

    #[test]
    fn filters_split_on_the_first_colon_only() {
        let f = parse_filters(Some("prompt:summarize@v3,model:gpt-5.4"), false)
            .ok()
            .expect("parsed");
        assert_eq!(
            f,
            vec![
                (Dimension::Prompt, "summarize@v3".to_string()),
                (Dimension::Model, "gpt-5.4".to_string()),
            ]
        );
        assert!(parse_filters(Some("no-colon"), false).is_err());
        assert!(
            parse_filters(Some("day:2026-01-01"), true).is_err(),
            "a day is a window, not a filter"
        );
        assert!(parse_filters(None, false).ok().expect("none").is_empty());
    }
}
