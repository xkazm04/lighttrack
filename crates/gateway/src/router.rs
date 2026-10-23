//! The axum router and the two small read surfaces beside the completions endpoint.

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::chat;
use crate::state::AppState;

pub fn build(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat::chat))
        .with_state(state)
}

/// Liveness plus the state an operator wants at a glance: which seats are on hold.
async fn health(State(st): State<AppState>) -> Json<Value> {
    let cooldowns: Vec<Value> = st
        .cooldowns
        .snapshot()
        .into_iter()
        .map(|(p, secs)| json!({ "provider": p, "remaining_secs": secs }))
        .collect();
    Json(json!({
        "ok": true,
        "routes": st.cfg.routes.len(),
        "telemetry": st.telemetry.is_some(),
        "dev": st.dev,
        "cooldowns": cooldowns,
    }))
}

/// The OpenAI models list, so a client that enumerates models before calling sees the routes.
async fn models(State(st): State<AppState>) -> Json<Value> {
    let data: Vec<Value> = st
        .cfg
        .routes
        .iter()
        .map(|(name, r)| {
            json!({
                "id": name,
                "object": "model",
                "owned_by": "lighttrack",
                "lighttrack": { "primary": r.primary, "fallback": r.fallback },
            })
        })
        .collect();
    Json(json!({ "object": "list", "data": data }))
}
