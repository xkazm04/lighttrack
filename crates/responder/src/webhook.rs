//! The `/webhook` endpoint: receive a LightTrack alert, pull out the `error_spike` payload, and hand
//! it to the pipeline on a detached task so we ack the POST immediately (investigations are slow).

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::Value;

use crate::pipeline;
use crate::state::AppState;

/// The `spike` object LightTrack's alerter emits for an `error_spike` event.
#[derive(Deserialize, Clone)]
pub(crate) struct Spike {
    pub project_id: String,
    #[serde(default)]
    pub count: Option<u32>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

pub(crate) async fn receive(State(st): State<AppState>, Json(body): Json<Value>) -> StatusCode {
    let event = body.get("event").and_then(Value::as_str).unwrap_or("(unknown)");
    let Some(spike_val) = body.get("spike") else {
        // Breach / forecast / relay-dead alerts share this endpoint; only error-spikes drive a run.
        println!("[responder] ignoring alert event='{event}' (no spike payload)");
        return StatusCode::OK;
    };
    let spike: Spike = match serde_json::from_value(spike_val.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[responder] malformed spike payload: {e}");
            return StatusCode::OK;
        }
    };
    tokio::spawn(pipeline::handle_spike(st.cfg, st.breaker, spike));
    StatusCode::OK
}
