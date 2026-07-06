//! The `/webhook` endpoint: receive a LightTrack alert, turn its `error_spike` (`spike`) or
//! `score_drop` (`drop`) payload into a [`Trigger`], and hand it to the pipeline on a detached task so
//! we ack the POST immediately (investigations are slow).

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

/// The `drop` object LightTrack's alerter emits for a `score_drop` (quality regression) event.
#[derive(Deserialize, Clone)]
pub(crate) struct Drop {
    pub project_id: String,
    #[serde(default)]
    pub rubric: Option<String>,
    #[serde(default)]
    pub recent_avg: Option<f64>,
    #[serde(default)]
    pub baseline_avg: Option<f64>,
    #[serde(default)]
    pub drop_pct: Option<f64>,
    #[serde(default)]
    pub scored_by: Option<String>,
}

/// What made the responder wake up: a failure spike or a quality regression.
pub(crate) enum Trigger {
    Error(Spike),
    Quality(Drop),
}

impl Trigger {
    pub(crate) fn project_id(&self) -> &str {
        match self {
            Trigger::Error(s) => &s.project_id,
            Trigger::Quality(d) => &d.project_id,
        }
    }
}

pub(crate) async fn receive(State(st): State<AppState>, Json(body): Json<Value>) -> StatusCode {
    let event = body.get("event").and_then(Value::as_str).unwrap_or("(unknown)");
    let trigger = if let Some(v) = body.get("spike") {
        match serde_json::from_value::<Spike>(v.clone()) {
            Ok(s) => Trigger::Error(s),
            Err(e) => return bad("spike", e),
        }
    } else if let Some(v) = body.get("drop") {
        match serde_json::from_value::<Drop>(v.clone()) {
            Ok(d) => Trigger::Quality(d),
            Err(e) => return bad("drop", e),
        }
    } else {
        // Breach / forecast / relay-dead alerts share this endpoint; only spikes/drops drive a run.
        println!("[responder] ignoring alert event='{event}' (no spike/drop payload)");
        return StatusCode::OK;
    };
    tokio::spawn(pipeline::handle_trigger(st.cfg, st.breaker, trigger));
    StatusCode::OK
}

fn bad(kind: &str, e: serde_json::Error) -> StatusCode {
    eprintln!("[responder] malformed {kind} payload: {e}");
    StatusCode::OK
}
