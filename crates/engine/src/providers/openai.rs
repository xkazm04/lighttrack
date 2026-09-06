//! The OpenAI Chat Completions adapter.
//!
//! Split out of `providers.rs` when the effort axis arrived: the request body is now built by a pure
//! function so a test can assert what actually goes on the wire — the `model` field in particular,
//! which used to receive whatever spec string the caller held, `@effort` suffix and all.

use std::time::Instant;

use serde_json::Value;

use lighttrack_core::Effort;

use super::{
    api_base, effort_unsupported, http_client, http_error, read_bounded, schema_state, send_error,
    PINNED_SEED,
};
use crate::{Determinism, EngineError, GenOutcome, Result};

/// The request body for one Chat Completions call. `model` is the **resolved** model — the caller
/// has already split any `@effort` suffix off it — so no `@`-suffixed string can reach the wire.
fn body(
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
    deterministic: bool,
    effort: Option<Effort>,
) -> Result<Value> {
    if let Some(level) = effort {
        return Err(effort_unsupported("openai", model, level));
    }
    let mut messages = Vec::new();
    if let Some(sys) = system_prompt {
        messages.push(serde_json::json!({ "role": "system", "content": sys }));
    }
    messages.push(serde_json::json!({ "role": "user", "content": input }));
    let mut body = serde_json::json!({ "model": model, "messages": messages });
    if let Some(sc) = schema {
        body["response_format"] = serde_json::json!({
            "type": "json_schema",
            "json_schema": { "name": "verdict", "strict": true, "schema": sc },
        });
    }
    if deterministic {
        // Some reasoning models reject `temperature`; generate_deterministic's fallback strips it.
        body["temperature"] = serde_json::json!(0.0);
        body["seed"] = serde_json::json!(PINNED_SEED);
    }
    Ok(body)
}

/// OpenAI Chat Completions. Key from OPENAI_API_KEY.
pub(super) fn generate(
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
    deterministic: bool,
    effort: Option<Effort>,
) -> Result<GenOutcome> {
    let key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| EngineError::Other("no OpenAI API key (set OPENAI_API_KEY)".into()))?;
    let body = body(model, system_prompt, input, schema, deterministic, effort)?;

    let started = Instant::now();
    let resp = http_client()?
        .post(format!(
            "{}/v1/chat/completions",
            api_base("LIGHTTRACK_OPENAI_BASE", "https://api.openai.com")
        ))
        .bearer_auth(&key)
        .json(&body)
        .send()
        .map_err(|e| send_error("openai", e))?;
    let latency_ms = Some(started.elapsed().as_millis() as u64);
    let status = resp.status();
    let headers = resp.headers().clone();
    let text = read_bounded(resp, "openai")?;
    if !status.is_success() {
        return Err(http_error("openai", status, &headers, text));
    }
    let v: Value = serde_json::from_str(&text)?;
    let usage = v.get("usage");
    // Read the stop condition BEFORE the payload is interpreted: a reasoning model that spends its
    // whole cap on hidden reasoning tokens reaches `content` as an empty string, which must read as
    // a cap hit, not as a normal stop that said nothing.
    if let Some((cap, reasoning_tokens)) = truncation(&v) {
        return Err(EngineError::Truncated {
            who: "openai".into(),
            cap,
            reasoning_tokens,
        });
    }
    let output = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if output.is_empty() {
        return Err(EngineError::EmptyCompletion {
            who: "openai".into(),
        });
    }
    Ok(GenOutcome {
        output,
        cost_usd: None,
        model: v
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| model.to_string()),
        latency_ms,
        input_tokens: usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(Value::as_u64),
        output_tokens: usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(Value::as_u64),
        schema: schema_state(schema),
        determinism: if deterministic {
            Determinism::Exact
        } else {
            Determinism::BestEffort
        },
    })
}

/// Whether a Chat Completions response was cut off by the token cap, and if so, the cap that
/// applied (`usage.completion_tokens`, which OpenAI defines to include reasoning tokens) plus the
/// share of it that went to hidden reasoning (`completion_tokens_details.reasoning_tokens`), when
/// the response reports one.
pub(super) fn truncation(v: &Value) -> Option<(u64, Option<u64>)> {
    if v.pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        != Some("length")
    {
        return None;
    }
    let usage = v.get("usage");
    let cap = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning_tokens = usage
        .and_then(|u| u.pointer("/completion_tokens_details/reasoning_tokens"))
        .and_then(Value::as_u64);
    Some((cap, reasoning_tokens))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// **The wire-body guarantee.** `gpt-5@high` reaches this adapter already split, and the body's
    /// `model` field carries the bare id — never a `@`-suffixed string, which OpenAI would answer
    /// with a 404 on a model nobody named.
    #[test]
    fn the_model_field_never_carries_an_effort_suffix() {
        let b = body("gpt-5", None, "hi", None, false, None).unwrap();
        assert_eq!(b["model"], json!("gpt-5"));
        assert!(
            !b["model"].as_str().unwrap().contains('@'),
            "no @-suffixed model may reach the wire: {}",
            b["model"]
        );
    }

    /// An effort this adapter has no knob for is an ERROR, never a dropped parameter: a run that
    /// asked for `@high` and quietly got the default would publish a scorecard column measuring
    /// something nobody requested.
    #[test]
    fn an_unmappable_effort_errors_rather_than_being_dropped() {
        let err = body("gpt-5", None, "hi", None, false, Some(Effort::High)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("openai"), "names the adapter: {msg}");
        assert!(msg.contains("gpt-5"), "names the model: {msg}");
        assert!(msg.contains("high"), "names the level: {msg}");
    }

    #[test]
    fn deterministic_pins_temperature_and_seed() {
        let b = body("gpt-4o", Some("be terse"), "hi", None, true, None).unwrap();
        assert_eq!(b["temperature"], json!(0.0));
        assert_eq!(b["seed"], json!(PINNED_SEED));
        assert_eq!(b["messages"][0]["role"], json!("system"));
        assert_eq!(b["messages"][1]["content"], json!("hi"));
    }

    /// `length` with an EMPTY answer: a reasoning model that spent its whole cap on hidden thinking.
    /// This must read as [`crate::EngineError::Truncated`], never as
    /// [`crate::EngineError::EmptyCompletion`] — the two look identical in the answer text and
    /// differ only in the stop condition, which is why it has to be read first.
    #[test]
    fn a_capped_call_with_zero_answer_tokens_is_truncated_not_empty() {
        let v = json!({
            "choices": [{ "finish_reason": "length", "message": { "content": "" } }],
            "usage": { "completion_tokens": 16000, "completion_tokens_details": { "reasoning_tokens": 16000 } }
        });
        assert_eq!(truncation(&v), Some((16000, Some(16000))));
    }

    /// `length` with a PARTIAL answer cut off mid-object: also truncation, and the caller gets `Err`
    /// before it ever sees the fragment, so it can never be salvaged into a JSON-shape failure that
    /// blames the model for our cap.
    #[test]
    fn a_capped_call_with_a_partial_answer_is_still_truncated() {
        let v = json!({
            "choices": [{ "finish_reason": "length", "message": { "content": "{\"score\": 0.8, \"reasoning\": \"cut off mid" } }],
            "usage": { "completion_tokens": 16000 }
        });
        assert_eq!(truncation(&v), Some((16000, None)));
    }

    /// A normal stop with an empty answer is NOT truncation, leaving the existing empty-completion
    /// path (a genuine "stopped normally, said nothing") untouched.
    #[test]
    fn a_normal_stop_with_an_empty_answer_is_not_truncation() {
        let v = json!({
            "choices": [{ "finish_reason": "stop", "message": { "content": "" } }],
            "usage": { "completion_tokens": 0 }
        });
        assert_eq!(truncation(&v), None);
    }
}
