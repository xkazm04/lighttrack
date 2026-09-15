//! `POST /v1/chat/completions` — the one endpoint an app points its OpenAI SDK at.

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use serde_json::{json, Value};

use lighttrack_core::new_id;

use crate::error::GatewayError;
use crate::failover::{self, AttemptKind, ChainResult, ChainRun};
use crate::routes;
use crate::state::AppState;
use crate::telemetry;
use crate::wire::{ChatRequest, Prompt};

/// Names the use case an event is filed under when the request used a literal model rather
/// than a route (a route's own name wins).
pub const USE_CASE_HEADER: &str = "x-lighttrack-use-case";
/// Dev-only: `exhausted:<provider>` pretends that seat hit its limit for this request.
pub const SIMULATE_HEADER: &str = "x-lighttrack-simulate";

pub async fn chat(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Result<Response, GatewayError> {
    if req.stream {
        return Err(GatewayError::unsupported(
            "stream: true is not supported yet; the CLIs behind this gateway answer whole",
        ));
    }
    if !req.tools.is_empty() {
        return Err(GatewayError::unsupported(
            "tools are not supported yet; the gateway forwards text and JSON schemas only",
        ));
    }
    let prompt = req.prompt().map_err(GatewayError::bad_request)?;
    let plan = routes::resolve(&st.cfg, &req.model)?;

    if let Some(tel) = &st.telemetry {
        let verdict = st.admission.check(tel).await;
        if verdict.throttled {
            return Err(GatewayError::limit_blocked(verdict.detail));
        }
    }

    let simulate = if st.dev {
        header_str(&headers, SIMULATE_HEADER)
            .and_then(|v| v.strip_prefix("exhausted:").map(str::to_string))
    } else {
        None
    };

    let request_id = new_id();
    let chain = plan.chain.clone();
    let st2 = st.clone();
    let prompt2 = prompt.clone();
    let result: ChainResult = tokio::task::spawn_blocking(move || {
        ChainRun {
            gen: st2.gen.as_ref(),
            cooldowns: &st2.cooldowns,
            default_hold: st2.default_hold(),
            simulate_exhausted: simulate.as_deref(),
        }
        .run(&chain, &prompt2)
    })
    .await
    .map_err(|e| GatewayError::internal(format!("generation task panicked: {e}")))?;

    if let Some(tel) = &st.telemetry {
        let name = plan
            .route
            .clone()
            .or_else(|| header_str(&headers, USE_CASE_HEADER));
        tel.emit(telemetry::events_for(
            tel,
            &request_id,
            plan.route.as_deref(),
            name.as_deref(),
            &prompt,
            &result,
        ));
    }

    let Some((target, out)) = result.outcome() else {
        let retry = result
            .attempts
            .iter()
            .filter_map(|a| match &a.kind {
                AttemptKind::SkippedCooling { remaining_secs } => Some(*remaining_secs),
                AttemptKind::Failed {
                    verdict: failover::Verdict::Exhausted(d),
                    ..
                } => Some(d.as_secs().max(1)),
                _ => None,
            })
            .min();
        return Err(GatewayError::chain_exhausted(
            failover::report(&result),
            retry,
        ));
    };

    let body = completion_body(
        &request_id,
        &plan.route,
        &target.spec(),
        out,
        &prompt,
        &result,
    );
    let mut resp = (StatusCode::OK, Json(body)).into_response();
    let h = resp.headers_mut();
    set(h, "x-lighttrack-request-id", &request_id);
    set(h, "x-lighttrack-served-by", &target.spec());
    set(
        h,
        "x-lighttrack-fell-back",
        if result.fell_back() { "1" } else { "0" },
    );
    Ok(resp)
}

fn completion_body(
    request_id: &str,
    route: &Option<String>,
    served_by: &str,
    out: &lighttrack_engine::GenOutcome,
    prompt: &Prompt,
    result: &ChainResult,
) -> Value {
    let input = out.input_tokens.unwrap_or(0);
    let output = out.output_tokens.unwrap_or(0);
    json!({
        "id": format!("chatcmpl-{request_id}"),
        "object": "chat.completion",
        "created": Utc::now().timestamp(),
        "model": served_by,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": out.output },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": input,
            "completion_tokens": output,
            "total_tokens": input + output,
            "completion_tokens_details": { "reasoning_tokens": out.reasoning_tokens.unwrap_or(0) }
        },
        "lighttrack": {
            "route": route,
            "served_by": served_by,
            "reported_model": out.model,
            "fell_back": result.fell_back(),
            "cost_usd": out.cost_usd,
            "determinism": out.determinism.as_str(),
            "schema": out.schema.as_str(),
            "transcript": prompt.transcript,
            "attempts": failover::report(result),
        }
    })
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn set(h: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        h.insert(header::HeaderName::from_static(name), v);
    }
}
