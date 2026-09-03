//! `GET /v1/costs/unpriced` — which models are carrying traffic nothing can cost.
//!
//! Ingest has always honoured the null-cost invariant: a call whose model is absent from the price
//! book stores `cost_usd = NULL`, never a zero we made up. Traces disclose it (`unpriced_spans`),
//! limit evaluation imputes for it (`CostEvidence.unpriced_calls`), and since M2 every rollup row
//! carries `unpriced_calls`. What nobody could do was *act* on it, because no surface said which
//! `(provider, model)` pairs were missing or how much traffic they carried.
//!
//! This is that surface, and it is written to be actionable rather than merely honest: rows ranked
//! by call count (the top row is the price worth adding first), the price book's own freshness
//! beside them, and a `notes` line naming the exact call that closes each row.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::{PriceBookPosture, UnpricedLedger};

use crate::error::ApiError;
use crate::events_query::parse_opt_ts;
use crate::guards::{authenticate, resolve_read_project};
use crate::prices::measure_posture;
use crate::state::{spawn_db, AppState};

/// How far back the ledger looks when the caller names no window. Long enough that a model
/// introduced last month still shows up, short enough that a pair nobody has called since spring
/// does not read as a live problem.
const DEFAULT_WINDOW_DAYS: i64 = 30;

#[derive(Deserialize)]
pub(crate) struct UnpricedParams {
    project: Option<String>,
    /// RFC3339 lower bound on server arrival time. Defaults to 30 days ago.
    since: Option<String>,
}

/// The ledger plus the book that produced it.
#[derive(Serialize)]
pub(crate) struct UnpricedResponse {
    #[serde(flatten)]
    ledger: UnpricedLedger,
    /// How fresh the rates that *did* apply are. A ledger read alongside a stale book is two
    /// separate reasons to distrust the cost numbers, and an operator should see both at once.
    price_book: PriceBookPosture,
}

pub(crate) async fn get_unpriced(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<UnpricedParams>,
) -> Result<Json<UnpricedResponse>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let since = parse_opt_ts("since", q.since.as_deref())?
        .unwrap_or_else(|| Utc::now() - Duration::days(DEFAULT_WINDOW_DAYS));

    let store = st.store.clone();
    let (rows, prices) = spawn_db(move || {
        let rows = store.list_unpriced(project.as_deref().into(), since)?;
        // Same call, same snapshot: the freshness reported beside the ledger is the freshness of
        // the book the ledger was measured against.
        let prices = store.list_prices()?;
        Ok((rows, prices))
    })
    .await?;

    Ok(Json(UnpricedResponse {
        ledger: UnpricedLedger::new(since, rows),
        price_book: measure_posture(&prices),
    }))
}
