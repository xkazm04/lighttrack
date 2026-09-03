//! The **bare Anthropic Messages API** judge path.
//!
//! The default judge provider is `anthropic`, which until now always meant the `claude -p`
//! subprocess. That path exposes no sampling knobs, so out of the box "agreement" partly measured
//! sampling noise — and every call re-pays for the CLI's auto-loaded context (see `DECISIONS.md`
//! D9: ~40k tokens of CLAUDE.md/skills/MCP, billed as cache creation, $0.02–0.10 per judge call
//! regardless of payload size).
//!
//! When `ANTHROPIC_API_KEY` is set we call `POST /v1/messages` directly instead: no auto-loaded
//! context, `temperature: 0` requested, and structured output enforced by a **forced tool call**
//! (the model must emit `verdict` with our schema as its `input_schema`), which is stricter than
//! parsing JSON out of prose. The CLI remains the fallback when no key is set, because subscription
//! users authenticate through its OAuth and have no key to give us.
//!
//! Two honest residuals, both stamped on the outcome as [`Determinism::BestEffort`]:
//! - **The Anthropic API exposes no `seed`.** `temperature: 0` is the only sampling control there
//!   is, so a verdict is reproducible by convention, not by contract (OpenAI and Gemini take both).
//! - **Some model/parameter combinations reject `temperature`** (a 400). We detect that response and
//!   retry once without it rather than hard-failing or silently dropping the schema. The trigger is
//!   the API's answer, never a hard-coded model list.

use std::time::Instant;

use serde_json::Value;

use crate::{Determinism, EngineError, GenOutcome, Result};

/// Env var that switches the `anthropic` provider onto this path.
pub(crate) const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
/// Pinned wire version — the Messages API requires it on every request.
const API_VERSION: &str = "2023-06-01";
/// Output ceiling for a judge verdict. A verdict is a small JSON object; this is generous headroom,
/// not a target.
const MAX_TOKENS: u64 = 4096;
/// The forced tool the judge answers through when a schema is supplied.
const VERDICT_TOOL: &str = "verdict";

/// True when the bare API path is available (a key is present in the environment).
pub(crate) fn available() -> bool {
    std::env::var(API_KEY_ENV).is_ok_and(|k| !k.trim().is_empty())
}

/// Resolve a CLI-style model alias to a Messages API model id. The `claude -p` aliases (`haiku`,
/// `sonnet`, `opus`) don't exist on the API, so a judge spec written for the CLI would 404 here.
/// Anything already looking like a model id passes through untouched.
fn resolve_model(model: &str) -> &str {
    match model {
        "haiku" => "claude-haiku-4-5",
        "sonnet" => "claude-sonnet-5",
        "opus" => "claude-opus-5",
        other => other,
    }
}

/// Generate one completion through the bare Messages API.
pub(crate) fn generate(
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
    deterministic: bool,
) -> Result<GenOutcome> {
    let key = std::env::var(API_KEY_ENV)
        .map_err(|_| EngineError::Other(format!("no Anthropic API key (set {API_KEY_ENV})")))?;
    let resolved = resolve_model(model);

    match send(
        &key,
        resolved,
        model,
        system_prompt,
        input,
        schema,
        deterministic,
    ) {
        // Some model/parameter combinations answer a `temperature` with a 400 (extended-thinking
        // configurations in particular). Detect it from the response rather than from a model
        // allowlist. Retry once *keeping the schema* — dropping to a schema-less prose call here
        // would trade a determinism residual for a structured-output regression.
        Err(EngineError::BadRequest { status, body, .. })
            if deterministic && body.contains("temperature") =>
        {
            eprintln!(
                "[judge] anthropic model '{resolved}' rejects temperature (HTTP {status}); \
                 retrying without it — determinism is best-effort for this run"
            );
            send(&key, resolved, model, system_prompt, input, schema, false)
        }
        other => other,
    }
}

#[allow(clippy::too_many_arguments)]
fn send(
    key: &str,
    resolved: &str,
    requested: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
    deterministic: bool,
) -> Result<GenOutcome> {
    let mut body = serde_json::json!({
        "model": resolved,
        "max_tokens": MAX_TOKENS,
        "messages": [{ "role": "user", "content": input }],
    });
    if let Some(sys) = system_prompt {
        body["system"] = serde_json::json!(sys);
    }
    if let Some(sc) = schema {
        // Forced tool use is the API's strictest structured-output shape: the model cannot answer
        // in prose, and `tool_use.input` comes back as an object rather than text to re-parse.
        body["tools"] = serde_json::json!([{
            "name": VERDICT_TOOL,
            "description": "Return the evaluation verdict.",
            "input_schema": sc,
        }]);
        body["tool_choice"] = serde_json::json!({ "type": "tool", "name": VERDICT_TOOL });
    }
    if deterministic {
        // The Anthropic API has no `seed`; temperature is the whole sampling surface here.
        body["temperature"] = serde_json::json!(0.0);
    }

    let started = Instant::now();
    let resp = crate::providers::http_client()?
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", key)
        .header("anthropic-version", API_VERSION)
        .json(&body)
        .send()
        .map_err(|e| crate::providers::send_error("anthropic", e))?;
    let latency_ms = Some(started.elapsed().as_millis() as u64);
    let status = resp.status();
    // Cloned before `read_bounded` consumes the response: a 429's stated schedule is in here.
    let headers = resp.headers().clone();
    let text = crate::providers::read_bounded(resp, "anthropic")?;
    if !status.is_success() {
        return Err(crate::providers::http_error(
            "anthropic",
            status,
            &headers,
            text,
        ));
    }
    let v: Value = serde_json::from_str(&text)?;
    let output = completion_text(&v, schema.is_some());
    if output.is_empty() {
        return Err(EngineError::EmptyCompletion {
            who: "anthropic".into(),
        });
    }
    let usage = v.get("usage");
    Ok(GenOutcome {
        output,
        // The Messages API returns no dollar cost; the caller prices it from the DB price book.
        cost_usd: None,
        model: v
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| requested.to_string()),
        latency_ms,
        input_tokens: usage
            .and_then(|u| u.get("input_tokens"))
            .and_then(Value::as_u64),
        output_tokens: usage
            .and_then(|u| u.get("output_tokens"))
            .and_then(Value::as_u64),
        // Temperature-pinned, but Anthropic exposes no seed — reproducible by convention only.
        determinism: Determinism::BestEffort,
    })
}

/// Pull the verdict out of a Messages response: the forced tool call's `input` (serialized, so the
/// existing JSON extraction sees clean JSON), else the concatenated text blocks.
fn completion_text(v: &Value, expect_tool: bool) -> String {
    let blocks = v.get("content").and_then(Value::as_array);
    if expect_tool {
        if let Some(input) = blocks.and_then(|bs| {
            bs.iter()
                .find(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                .and_then(|b| b.get("input"))
        }) {
            return input.to_string();
        }
    }
    blocks
        .map(|bs| {
            bs.iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cli_aliases_resolve_to_api_model_ids() {
        assert_eq!(resolve_model("haiku"), "claude-haiku-4-5");
        assert_eq!(resolve_model("sonnet"), "claude-sonnet-5");
        assert_eq!(resolve_model("opus"), "claude-opus-5");
        // A real model id is passed through untouched.
        assert_eq!(resolve_model("claude-haiku-4-5"), "claude-haiku-4-5");
    }

    #[test]
    fn forced_tool_input_is_the_verdict() {
        let resp = json!({
            "content": [
                { "type": "text", "text": "ignore me" },
                { "type": "tool_use", "name": "verdict", "input": { "x": { "score": 0.4 } } }
            ]
        });
        let out = completion_text(&resp, true);
        assert!(out.contains("\"score\":0.4"), "got {out}");
    }

    #[test]
    fn falls_back_to_text_blocks_without_a_schema() {
        let resp = json!({ "content": [
            { "type": "text", "text": "{\"x\":" },
            { "type": "text", "text": "{\"score\":0.4}}" }
        ]});
        assert_eq!(completion_text(&resp, false), "{\"x\":{\"score\":0.4}}");
        // …and when a tool was expected but the model answered in prose, text is still recovered.
        assert_eq!(completion_text(&resp, true), "{\"x\":{\"score\":0.4}}");
    }

    #[test]
    fn empty_content_yields_empty_text() {
        assert_eq!(completion_text(&json!({}), true), "");
        assert_eq!(completion_text(&json!({ "content": [] }), false), "");
    }
}
