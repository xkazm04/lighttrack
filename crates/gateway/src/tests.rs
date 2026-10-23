//! The wired router end to end, with a scripted generator in place of the CLIs.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use lighttrack_engine::EngineError;

use crate::admission::Admission;
use crate::config::GatewayConfig;
use crate::cooldown::Cooldowns;
use crate::failover::tests::Script;
use crate::router;
use crate::state::AppState;

fn app(script: Script, dev: bool) -> axum::Router {
    let cfg: GatewayConfig = toml::from_str(
        r#"
        cooldown_secs = 120
        [defaults]
        fallback = ["codex/gpt-5.5"]
        [routes.summarize]
        primary = "anthropic/claude-sonnet-5@medium"
        fallback = ["codex/gpt-5.5@medium"]
        "#,
    )
    .unwrap();
    router::build(AppState {
        cfg: Arc::new(cfg),
        gen: Arc::new(script),
        cooldowns: Arc::new(Cooldowns::default()),
        telemetry: None,
        admission: Arc::new(Admission::default()),
        dev,
    })
}

async fn post(
    app: &axum::Router,
    body: Value,
    extra: &[(&str, &str)],
) -> (StatusCode, Value, axum::http::HeaderMap) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    for (k, v) in extra {
        req = req.header(*k, *v);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap(), headers)
}

fn ask(model: &str) -> Value {
    json!({ "model": model, "messages": [{ "role": "user", "content": "hi" }] })
}

#[tokio::test]
async fn a_route_answers_in_the_openai_shape() {
    let app = app(Script::new(vec![("anthropic", Ok("hello"))]), false);
    let (status, body, headers) = post(&app, ask("summarize"), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "hello");
    assert_eq!(body["model"], "anthropic/claude-sonnet-5@medium");
    assert_eq!(body["usage"]["total_tokens"], 13);
    assert_eq!(body["lighttrack"]["fell_back"], false);
    assert_eq!(headers["x-lighttrack-fell-back"], "0");
}

#[tokio::test]
async fn a_usage_limit_on_the_primary_is_served_by_the_fallback_and_says_so() {
    let app = app(
        Script::new(vec![
            (
                "anthropic",
                Err(EngineError::NonZero {
                    code: 1,
                    stderr: "Claude usage limit reached".into(),
                }),
            ),
            ("codex", Ok("from gpt")),
        ]),
        false,
    );
    let (status, body, headers) = post(&app, ask("summarize"), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["choices"][0]["message"]["content"], "from gpt");
    assert_eq!(body["lighttrack"]["fell_back"], true);
    assert_eq!(body["lighttrack"]["attempts"][0]["outcome"], "exhausted");
    assert_eq!(headers["x-lighttrack-served-by"], "codex/gpt-5.5@medium");
    assert_eq!(headers["x-lighttrack-fell-back"], "1");
}

#[tokio::test]
async fn every_seat_exhausted_is_a_503_with_retry_after() {
    let limit = || EngineError::RateLimited {
        who: "x".into(),
        retry_after: Some(std::time::Duration::from_secs(30)),
    };
    let app = app(
        Script::new(vec![("anthropic", Err(limit())), ("codex", Err(limit()))]),
        false,
    );
    let (status, body, headers) = post(&app, ask("summarize"), &[]).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "upstream_failed");
    assert_eq!(body["lighttrack"]["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(headers["retry-after"], "30");
}

#[tokio::test]
async fn the_simulate_header_only_works_in_dev_mode() {
    let script = || Script::new(vec![("anthropic", Ok("real")), ("codex", Ok("stand-in"))]);
    let hdr = [("x-lighttrack-simulate", "exhausted:anthropic")];
    let (_, body, _) = post(&app(script(), false), ask("summarize"), &hdr).await;
    assert_eq!(body["choices"][0]["message"]["content"], "real");
    let (_, body, _) = post(&app(script(), true), ask("summarize"), &hdr).await;
    assert_eq!(body["choices"][0]["message"]["content"], "stand-in");
    assert_eq!(body["lighttrack"]["fell_back"], true);
}

#[tokio::test]
async fn unknown_models_and_tools_on_a_cli_route_are_refused_before_any_seat_is_spent() {
    let app = app(Script::new(vec![]), false);
    let (status, body, _) = post(&app, ask("gpt-4o"), &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "model_not_found");

    // The route's primary is a CLI: tools cannot pass through, and the refusal names it.
    let mut tools = ask("summarize");
    tools["tools"] = json!([{ "type": "function", "function": { "name": "now" } }]);
    let (status, body, _) = post(&app, tools, &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "unsupported");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("anthropic/claude-sonnet-5@medium"));
}

#[tokio::test]
async fn tools_pass_through_an_openai_shaped_target_and_come_back_as_tool_calls() {
    let app = app(Script::new(vec![("openai", Ok("TOOL:now"))]), false);
    let mut req = ask("openai/gpt-5.5");
    req["tools"] = json!([{ "type": "function", "function": { "name": "now" } }]);
    let (status, body, _) = post(&app, req, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(body["choices"][0]["message"]["content"], Value::Null);
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "now"
    );
}

#[tokio::test]
async fn a_conversation_reaches_the_provider_as_turns() {
    let script = Script::new(vec![("anthropic", Ok("ok"))]);
    let app = app(script, false);
    let req = json!({ "model": "summarize", "messages": [
        { "role": "system", "content": "s" },
        { "role": "user", "content": "a" },
        { "role": "assistant", "content": "b" },
        { "role": "user", "content": "c" }
    ]});
    let (status, body, _) = post(&app, req, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["lighttrack"]["transcript"], true);
}

#[tokio::test]
async fn stream_true_delivers_the_answer_as_sse_chunks() {
    let app = app(
        Script::new(vec![("anthropic", Ok("alpha beta gamma delta"))]),
        false,
    );
    let mut req = ask("summarize");
    req["stream"] = json!(true);
    let resp = app
        .oneshot(
            Request::post("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let text = String::from_utf8(
        to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(text.ends_with("data: [DONE]\n\n"));
    let chunks: Vec<Value> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|l| *l != "[DONE]")
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    let content: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(content, "alpha beta gamma delta");
    assert_eq!(
        chunks[chunks.len() - 2]["choices"][0]["finish_reason"],
        "stop"
    );
    assert_eq!(chunks.last().unwrap()["usage"]["total_tokens"], 13);
}

#[tokio::test]
async fn a_literal_target_is_served_with_the_default_fallback_behind_it() {
    let app = app(
        Script::new(vec![
            (
                "anthropic",
                Err(EngineError::Timeout {
                    who: "claude".into(),
                }),
            ),
            ("codex", Ok("ok")),
        ]),
        false,
    );
    let (status, body, _) = post(&app, ask("anthropic/claude-opus-5@high"), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["lighttrack"]["route"], Value::Null);
    assert_eq!(body["lighttrack"]["attempts"][0]["outcome"], "transient");
    assert_eq!(body["model"], "codex/gpt-5.5");
}

#[tokio::test]
async fn models_lists_the_routes() {
    let app = app(Script::new(vec![]), false);
    let resp = app
        .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["data"][0]["id"], "summarize");
}
