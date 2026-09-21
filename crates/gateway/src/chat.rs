//! `POST /v1/chat/completions` — the one endpoint an app points its OpenAI SDK at.

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use serde_json::{json, Value};

use lighttrack_core::new_id;
use lighttrack_engine::{supports_tools, ChatOutcome};

use crate::error::GatewayError;
use crate::failover::{self, AttemptKind, ChainResult, ChainRun};
use crate::routes;
use crate::state::AppState;
use crate::stream;
use crate::telemetry;
use crate::wire::{ChatCompletionRequest, Prepared};

/// Names the use case an event is filed under when the request used a literal model rather
/// than a route (a route's own name wins).
pub const USE_CASE_HEADER: &str = "x-lighttrack-use-case";
/// Dev-only: `exhausted:<provider>` pretends that seat hit its limit for this request.
pub const SIMULATE_HEADER: &str = "x-lighttrack-simulate";

pub async fn chat(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChatCompletionRequest>,
) -> Result<Response, GatewayError> {
    let prepared = req.prepare().map_err(GatewayError::bad_request)?;
    let plan = routes::resolve(&st.cfg, &req.model)?;
    // Tools are refused up front, by target, rather than discovered one failed seat at a time:
    // a route whose fallback cannot run tools would otherwise serve tool-less prose during a
    // limit window and call it a fallback.
    if prepared.req.has_tools() {
        if let Some(t) = plan.chain.iter().find(|t| !supports_tools(&t.provider)) {
            return Err(GatewayError::unsupported(format!(
                "target '{}' cannot run tools (only OpenAI-shaped providers pass them through); \
                 route '{}' is not a tool-capable route",
                t.spec(),
                req.model
            )));
        }
    }

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
    let req2 = prepared.req.clone();
    let result: ChainResult = tokio::task::spawn_blocking(move || {
        ChainRun {
            gen: st2.gen.as_ref(),
            cooldowns: &st2.cooldowns,
            default_hold: st2.default_hold(),
            simulate_exhausted: simulate.as_deref(),
        }
        .run(&chain, &req2)
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
            &prepared,
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
        &prepared,
        &result,
    );
    let mut resp = if req.stream {
        stream::response(&body)
    } else {
        (StatusCode::OK, Json(body)).into_response()
    };
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
    out: &ChatOutcome,
    prepared: &Prepared,
    result: &ChainResult,
) -> Value {
    let gen = &out.gen;
    let input = gen.input_tokens.unwrap_or(0);
    let output = gen.output_tokens.unwrap_or(0);
    let mut message = json!({ "role": "assistant", "content": gen.output });
    // A tool request is the OpenAI shape verbatim: `content: null`, `tool_calls`, and the stop
    // reason that tells an SDK to run them.
    let finish_reason = match &out.tool_calls {
        Some(calls) => {
            message["tool_calls"] = calls.clone();
            if gen.output.is_empty() {
                message["content"] = Value::Null;
            }
            "tool_calls".to_string()
        }
        None => out
            .finish_reason
            .clone()
            .unwrap_or_else(|| "stop".to_string()),
    };
    json!({
        "id": format!("chatcmpl-{request_id}"),
        "object": "chat.completion",
        "created": Utc::now().timestamp(),
        "model": served_by,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
        "usage": {
            "prompt_tokens": input,
            "completion_tokens": output,
            "total_tokens": input + output,
            "completion_tokens_details": { "reasoning_tokens": gen.reasoning_tokens.unwrap_or(0) }
        },
        "lighttrack": {
            "route": route,
            "served_by": served_by,
            "reported_model": gen.model,
            "fell_back": result.fell_back(),
            "cost_usd": gen.cost_usd,
            "determinism": gen.determinism.as_str(),
            "schema": gen.schema.as_str(),
            "transcript": prepared.transcript,
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
