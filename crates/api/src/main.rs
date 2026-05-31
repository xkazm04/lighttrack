//! LightTrack API — ingest + query REST service (Phase 1).
//!
//! Routes:
//!   GET  /health
//!   POST /v1/events          ingest one normalized event (cost computed server-side)
//!   GET  /v1/events?project=&limit=
//!   GET  /v1/costs?project=  rollup grouped by project + provider + model
//!
//! Config via env: LIGHTTRACK_BIND (127.0.0.1:8787), LIGHTTRACK_DB (data/lighttrack.db),
//! LIGHTTRACK_PRICING (config/pricing.json). Auth (API keys) arrives in Phase 2.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::{LlmEvent, PriceBook};
use lighttrack_store::{CostRow, SqliteStore, Store, StoreError};

#[derive(Clone)]
struct AppState {
    store: Arc<SqliteStore>,
    prices: Arc<PriceBook>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bind = env_or("LIGHTTRACK_BIND", "127.0.0.1:8787");
    let db = env_or("LIGHTTRACK_DB", "data/lighttrack.db");
    let pricing = env_or("LIGHTTRACK_PRICING", "config/pricing.json");

    let prices = match std::fs::read_to_string(&pricing) {
        Ok(s) => PriceBook::from_json_str(&s).unwrap_or_else(|e| {
            eprintln!("pricing parse error: {e}; using empty book");
            PriceBook::default()
        }),
        Err(_) => {
            eprintln!("pricing file '{pricing}' not found; using empty book");
            PriceBook::default()
        }
    };

    let store = SqliteStore::open(&db)?;
    let state = AppState {
        store: Arc::new(store),
        prices: Arc::new(prices),
    };

    println!(
        "lighttrack-api v{} on http://{bind}  (db={db}, {} priced models)",
        env!("CARGO_PKG_VERSION"),
        state.prices.len()
    );

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/events", post(post_event).get(get_events))
        .route("/v1/costs", get(get_costs))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Serialize)]
struct IngestResponse {
    id: String,
    cost_usd: Option<f64>,
    ts: DateTime<Utc>,
}

async fn post_event(
    State(st): State<AppState>,
    Json(mut ev): Json<LlmEvent>,
) -> Result<Json<IngestResponse>, ApiError> {
    // Fill cost from the price book if the client didn't provide it.
    ev.ensure_cost(st.prices.as_ref());

    let store = st.store.clone();
    let to_insert = ev.clone();
    spawn_db(move || store.insert_event(&to_insert)).await?;

    Ok(Json(IngestResponse {
        id: ev.id,
        cost_usd: ev.cost_usd,
        ts: ev.ts,
    }))
}

#[derive(Deserialize)]
struct EventsParams {
    project: Option<String>,
    limit: Option<usize>,
}

async fn get_events(
    State(st): State<AppState>,
    Query(q): Query<EventsParams>,
) -> Result<Json<Vec<LlmEvent>>, ApiError> {
    let store = st.store.clone();
    let project = q.project;
    let limit = q.limit.unwrap_or(50).min(1000);
    let events = spawn_db(move || store.list_events(project.as_deref(), limit)).await?;
    Ok(Json(events))
}

#[derive(Deserialize)]
struct CostParams {
    project: Option<String>,
}

async fn get_costs(
    State(st): State<AppState>,
    Query(q): Query<CostParams>,
) -> Result<Json<Vec<CostRow>>, ApiError> {
    let store = st.store.clone();
    let project = q.project;
    let rows = spawn_db(move || store.cost_summary(project.as_deref())).await?;
    Ok(Json(rows))
}

/// Run a blocking store call on the blocking pool and flatten the two error layers.
async fn spawn_db<T, F>(f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, StoreError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(format!("task join error: {e}")))?
        .map_err(ApiError::from)
}

/// JSON error response.
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn internal(m: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: m.into(),
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        ApiError::internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(serde_json::json!({ "error": self.message }))).into_response()
    }
}
