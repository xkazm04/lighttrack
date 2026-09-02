//! `GET /v1/collective/contributions` — the contributor-side ledger (admin).
//!
//! Admin-only, like `GET /digest`, and for the same reason: the ledger spans every project's
//! consent decision (`projects_included` / `projects_excluded` are instance-wide counts), so it is
//! operator information, not one tenant's data.
//!
//! Paged with the same opaque keyset cursor every other listing uses, returned in the
//! `X-Next-Cursor` header rather than the body so the row shape stays exactly `ContributionRecord`.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, HeaderName, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use lighttrack_core::ContributionRecord;
use lighttrack_store::codec::{encode_event_cursor, fmt_ts};

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::state::{spawn_db, AppState};

/// The header the next page's cursor comes back in — the same one the events listing uses.
pub(crate) const NEXT_CURSOR_HEADER: &str = "x-next-cursor";

#[derive(Deserialize)]
pub(crate) struct LedgerParams {
    /// Page size; omitted or `0` ⇒ the store's default.
    limit: Option<usize>,
    /// Keyset cursor from a previous page's `X-Next-Cursor`.
    cursor: Option<String>,
}

pub(crate) async fn get_contributions(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<LedgerParams>,
) -> Result<Response, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let limit = q.limit.unwrap_or(0);
    let cursor = q.cursor.clone();
    let store = st.store.clone();
    let rows = spawn_db(move || store.list_contributions(limit, cursor.as_deref())).await?;
    Ok(page(rows).into_response())
}

/// Attach the next cursor when the page came back full — the same "a short page is the last page"
/// rule the other keyset listings use, so a client's loop terminates without a second empty call.
fn page(rows: Vec<ContributionRecord>) -> impl IntoResponse {
    let next = rows
        .last()
        .map(|c| encode_event_cursor(&fmt_ts(c.created_at), &c.id));
    let mut headers = HeaderMap::new();
    if let Some(c) = next {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(NEXT_CURSOR_HEADER.as_bytes()),
            HeaderValue::from_str(&c),
        ) {
            headers.insert(name, value);
        }
    }
    (headers, Json(rows))
}
