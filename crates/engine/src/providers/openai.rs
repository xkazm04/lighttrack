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
use crate::chat::{ChatOutcome, ChatRequest};
use crate::{Determinism, EngineError, GenOutcome, Result};

/// Our effort level as a Chat Completions `reasoning_effort` value.
///
/// The OpenAI scale is `low | medium | high` and stops there; ours goes two rungs further. Those two
/// are **refused**, not folded onto `high`: folding would make `gpt-5@xhigh` and `gpt-5@high` send
/// byte-identical requests while the leaderboard printed them as two rows, and a scorecard column
/// that measures the same thing twice under two names is worse than a missing one.
fn reasoning_effort(model: &str, effort: Effort) -> Result<&'static str> {
    match effort {
        Effort::Low => Ok("low"),
        Effort::Medium => Ok("medium"),
        Effort::High => Ok("high"),
        Effort::XHigh | Effort::Max => Err(effort_unsupported(
            "openai",
            model,
            effort,
            "OpenAI's `reasoning_effort` scale ends at 'high', so folding this level onto it would \
             send byte-identical requests for two differently-labelled targets — declare '@high' if \
             that is what you want measured",
        )),
    }
}

/// The request body for one Chat Completions call. `model` is the **resolved** model — the caller
/// has already split any `@effort` suffix off it — so no `@`-suffixed string can reach the wire.
fn body(
    model: &str,
    req: &ChatRequest,
    deterministic: bool,
    effort: Option<Effort>,
) -> Result<Value> {
    let mut body = serde_json::json!({ "model": model, "messages": req.openai_messages() });
    if let Some(level) = effort {
        body["reasoning_effort"] = serde_json::json!(reasoning_effort(model, level)?);
    }
    // Caller tools ride through untouched: this is their native wire shape.
    if let Some(tools) = &req.tools {
        body["tools"] = tools.clone();
        if let Some(choice) = &req.tool_choice {
            body["tool_choice"] = choice.clone();
        }
    }
    if let Some(sc) = &req.schema {
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
    req: &ChatRequest,
    deterministic: bool,
    effort: Option<Effort>,
) -> Result<ChatOutcome> {
    let key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| EngineError::Other("no OpenAI API key (set OPENAI_API_KEY)".into()))?;
    let body = body(model, req, deterministic, effort)?;

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
    let (output, tool_calls, finish_reason) = message_parts(&v);
    // A tool request is an answer with no text; only a stop with neither is empty.
    if output.is_empty() && tool_calls.is_none() {
        return Err(EngineError::EmptyCompletion {
            who: "openai".into(),
        });
    }
    let gen = GenOutcome {
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
        reasoning_tokens: reasoning_tokens(usage),
        schema: schema_state(req.schema.as_ref()),
        determinism: if deterministic {
            Determinism::Exact
        } else {
            Determinism::BestEffort
        },
    };
    Ok(ChatOutcome {
        gen,
        tool_calls,
        finish_reason,
    })
}

/// The first choice's text, its `tool_calls` (when the model asked for any), and the stated
/// `finish_reason`. Shared with the OpenRouter adapter, whose responses are this shape.
pub(super) fn message_parts(v: &Value) -> (String, Option<Value>, Option<String>) {
    let choice = v.pointer("/choices/0");
    let output = choice
        .and_then(|c| c.pointer("/message/content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let tool_calls = choice
        .and_then(|c| c.pointer("/message/tool_calls"))
        .filter(|t| t.as_array().is_some_and(|a| !a.is_empty()))
        .cloned();
    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .map(str::to_string);
    (output, tool_calls, finish_reason)
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
    Some((cap, reasoning_tokens(usage)))
}

/// The hidden-reasoning share of a Chat Completions call (`completion_tokens_details.reasoning_tokens`),
/// already counted inside `completion_tokens`. `None` when the response reports no split. Shared with
/// the OpenRouter adapter, which normalizes every upstream onto this shape.
pub(super) fn reasoning_tokens(usage: Option<&Value>) -> Option<u64> {
    usage?
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one(input: &str) -> ChatRequest {
        ChatRequest::single(None, input, None)
    }

    /// **The wire-body guarantee.** `gpt-5@high` reaches this adapter already split, and the body's
    /// `model` field carries the bare id — never a `@`-suffixed string, which OpenAI would answer
    /// with a 404 on a model nobody named.
    #[test]
    fn the_model_field_never_carries_an_effort_suffix() {
        let b = body("gpt-5", &one("hi"), false, None).unwrap();
        assert_eq!(b["model"], json!("gpt-5"));
        assert!(
            !b["model"].as_str().unwrap().contains('@'),
            "no @-suffixed model may reach the wire: {}",
            b["model"]
        );
    }

    /// **The effort knob.** The three levels OpenAI names travel verbatim as `reasoning_effort`.
    #[test]
    fn the_three_mappable_levels_travel_as_reasoning_effort() {
        for (level, wire) in [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
        ] {
            let b = body("gpt-5", &one("hi"), false, Some(level)).unwrap();
            assert_eq!(b["reasoning_effort"], json!(wire));
            assert_eq!(b["model"], json!("gpt-5"), "still the bare model id");
        }
        // No effort asked for: the parameter is absent, so an existing matrix's request is unchanged.
        let plain = body("gpt-4o", &one("hi"), false, None).unwrap();
        assert!(plain.get("reasoning_effort").is_none());
    }

    /// A level this adapter cannot express is an ERROR, never a fold onto the nearest rung: `xhigh`
    /// silently sent as `high` would give the leaderboard two rows whose requests were identical.
    #[test]
    fn a_level_above_openais_scale_errors_rather_than_folding_onto_high() {
        for level in [Effort::XHigh, Effort::Max] {
            let err = body("gpt-5", &one("hi"), false, Some(level)).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("openai"), "names the adapter: {msg}");
            assert!(msg.contains("gpt-5"), "names the model: {msg}");
            assert!(msg.contains(level.as_str()), "names the level: {msg}");
        }
    }

    #[test]
    fn deterministic_pins_temperature_and_seed() {
        let b = body(
            "gpt-4o",
            &ChatRequest::single(Some("be terse"), "hi", None),
            true,
            None,
        )
        .unwrap();
        assert_eq!(b["temperature"], json!(0.0));
        assert_eq!(b["seed"], json!(PINNED_SEED));
        assert_eq!(b["messages"][0]["role"], json!("system"));
        assert_eq!(b["messages"][1]["content"], json!("hi"));
        // Asking for effort must not weaken the determinism request: the `Exact` stamp still rests
        // on exactly the two parameters it always did.
        let with_effort = body("gpt-5", &one("hi"), true, Some(Effort::Low)).unwrap();
        assert_eq!(with_effort["temperature"], json!(0.0));
        assert_eq!(with_effort["seed"], json!(PINNED_SEED));
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

    /// A reasoning model's hidden share is read off the usage details on a NORMAL stop too — not only
    /// inside a truncation error, which used to be the one place this number was ever looked at.
    #[test]
    fn reasoning_tokens_are_read_on_a_normal_stop() {
        let usage = json!({ "completion_tokens": 900, "completion_tokens_details": { "reasoning_tokens": 850 } });
        assert_eq!(reasoning_tokens(Some(&usage)), Some(850));
        assert_eq!(
            reasoning_tokens(Some(&json!({ "completion_tokens": 12 }))),
            None,
            "no split reported is unknown, never zero"
        );
        assert_eq!(reasoning_tokens(None), None);
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
