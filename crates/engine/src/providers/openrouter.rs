//! The OpenRouter adapter — one API in front of every lab's models.
//!
//! Why this earns a place beside the three first-party adapters instead of being an OpenAI base-URL
//! re-point: it normalizes the two things this framework measures and the OpenAI shape cannot carry.
//! `reasoning.effort` takes **all five** of our levels — including `xhigh` and `max`, which the
//! OpenAI adapter has to refuse — so a matrix can walk the whole ladder on a model whose own API
//! stops short of it. And its usage block reports the hidden-reasoning share for every upstream,
//! including ones whose native API reports none, which is what a thinking measure needs.
//!
//! It also returns a **dollar cost** per call, so a target routed through it is priced without a
//! price-book entry rather than excluded from the cost–quality frontier as unpriced.
//!
//! What it costs, stated rather than discovered: which upstream serves a call is OpenRouter's
//! decision, so a pinned request is `best-effort` reproducible at most — see [`generate`].

use std::time::Instant;

use serde_json::Value;

use lighttrack_core::Effort;

use super::{
    api_base, http_client, http_error, read_bounded, request_timeout, schema_state, send_error,
    PINNED_SEED,
};
use crate::{Determinism, EngineError, GenOutcome, Result};

/// The provider id that routes here. Matched as an **id**, never a family: OpenRouter is a gateway,
/// so the lab that matters for bias control is whichever one the *model name* names.
pub(super) const PROVIDER_ID: &str = "openrouter";

/// The request body for one chat completion. `model` is the resolved spec — the caller has already
/// split any `@effort` suffix off it — and keeps its `lab/model` shape, which is how OpenRouter names
/// a model and must not be mistaken for a prefix to strip.
fn body(
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
    deterministic: bool,
    effort: Option<Effort>,
) -> Value {
    let mut messages = Vec::new();
    if let Some(sys) = system_prompt {
        messages.push(serde_json::json!({ "role": "system", "content": sys }));
    }
    messages.push(serde_json::json!({ "role": "user", "content": input }));
    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
        // Ask for the accounting explicitly: without it the response carries no cost and no
        // reasoning split, which are the two things this adapter exists to collect.
        "usage": { "include": true },
    });
    if let Some(level) = effort {
        // All five levels travel verbatim. This is the only adapter with no gap in the ladder.
        body["reasoning"] = serde_json::json!({ "effort": level.as_str() });
    }
    if let Some(sc) = schema {
        body["response_format"] = serde_json::json!({
            "type": "json_schema",
            "json_schema": { "name": "verdict", "strict": true, "schema": sc },
        });
    }
    if deterministic {
        body["temperature"] = serde_json::json!(0.0);
        body["seed"] = serde_json::json!(PINNED_SEED);
    }
    body
}

/// The dollar cost OpenRouter charged for a call, from its usage accounting. Non-finite or negative
/// values are dropped rather than trusted: the caller prices an absent cost from the book, which is
/// a known-unknown, while a nonsense number would be a silent wrong answer in every $ column.
fn cost_usd(v: &Value) -> Option<f64> {
    v.pointer("/usage/cost")
        .and_then(Value::as_f64)
        .filter(|c| c.is_finite() && *c >= 0.0)
}

/// Generate through OpenRouter. Key from `OPENROUTER_API_KEY`.
///
/// **Determinism is `best-effort`, even when pinning was asked for.** `temperature: 0` and a fixed
/// seed are sent, but OpenRouter chooses which upstream provider serves the request and they do not
/// all honour a seed — so claiming `exact` here would attach a contract to a call that has none. The
/// stamp is the honest one, and it is what a run's reproducibility is folded from.
pub(super) fn generate(
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
    deterministic: bool,
    effort: Option<Effort>,
) -> Result<GenOutcome> {
    let key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| EngineError::Other("no OpenRouter API key (set OPENROUTER_API_KEY)".into()))?;
    let body = body(model, system_prompt, input, schema, deterministic, effort);

    let started = Instant::now();
    let resp = http_client()?
        .post(format!(
            "{}/v1/chat/completions",
            api_base("LIGHTTRACK_OPENROUTER_BASE", "https://openrouter.ai/api")
        ))
        .bearer_auth(&key)
        // Per call, like the Anthropic path: a request at `xhigh`/`max` is a request for more
        // deliberation, and the shared client's ceiling was measured against default-effort calls.
        .timeout(request_timeout(effort))
        .json(&body)
        .send()
        .map_err(|e| send_error(PROVIDER_ID, e))?;
    let latency_ms = Some(started.elapsed().as_millis() as u64);
    let status = resp.status();
    let headers = resp.headers().clone();
    let text = read_bounded(resp, PROVIDER_ID)?;
    if !status.is_success() {
        return Err(http_error(PROVIDER_ID, status, &headers, text));
    }
    let v: Value = serde_json::from_str(&text)?;
    let usage = v.get("usage");
    // The stop condition before the payload, as everywhere else: a reasoning model that spent its
    // whole cap thinking reaches `content` as an empty string, and that is a cap hit rather than a
    // normal stop that said nothing. The shape is OpenAI's, so the reader is too.
    if let Some((cap, reasoning_tokens)) = super::openai::truncation(&v) {
        return Err(EngineError::Truncated {
            who: PROVIDER_ID.into(),
            cap,
            reasoning_tokens,
        });
    }
    let output = v
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if output.is_empty() {
        return Err(EngineError::EmptyCompletion {
            who: PROVIDER_ID.into(),
        });
    }
    Ok(GenOutcome {
        output,
        cost_usd: cost_usd(&v),
        // What actually served the call, which may name a lab the caller did not: the routed model
        // is the honest identity, and it is what the self-preference control reads.
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
        reasoning_tokens: super::openai::reasoning_tokens(usage),
        determinism: Determinism::BestEffort,
        schema: schema_state(schema),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// **The reason this adapter exists.** Every level of our ladder travels, including the two the
    /// OpenAI adapter must refuse — so `model@max` is a row a matrix can actually measure.
    #[test]
    fn all_five_effort_levels_travel_verbatim() {
        for level in Effort::ALL {
            let b = body(
                "anthropic/claude-opus-5",
                None,
                "hi",
                None,
                false,
                Some(level),
            );
            assert_eq!(b["reasoning"]["effort"], json!(level.as_str()));
        }
        // No effort asked for: the key is absent, so the provider's default applies and is not
        // reported as a level.
        let plain = body("openai/gpt-4o", None, "hi", None, false, None);
        assert!(plain.get("reasoning").is_none());
    }

    /// The model keeps its `lab/model` shape — that *is* the id here, not a prefix to strip — and
    /// usage accounting is always requested, because cost and the reasoning split ride on it.
    #[test]
    fn the_model_keeps_its_slash_and_usage_is_always_requested() {
        let b = body(
            "anthropic/claude-sonnet-5",
            Some("terse"),
            "hi",
            None,
            false,
            None,
        );
        assert_eq!(b["model"], json!("anthropic/claude-sonnet-5"));
        assert_eq!(b["usage"]["include"], json!(true));
        assert_eq!(b["messages"][0]["role"], json!("system"));
        assert_eq!(b["messages"][1]["content"], json!("hi"));
    }

    #[test]
    fn deterministic_pins_temperature_and_seed() {
        let b = body("openai/gpt-4o", None, "hi", None, true, Some(Effort::Low));
        assert_eq!(b["temperature"], json!(0.0));
        assert_eq!(b["seed"], json!(PINNED_SEED));
        // …and asking for effort does not disturb either.
        assert_eq!(b["reasoning"]["effort"], json!("low"));
        let loose = body("openai/gpt-4o", None, "hi", None, false, None);
        assert!(loose.get("temperature").is_none() && loose.get("seed").is_none());
    }

    #[test]
    fn a_schema_is_sent_as_a_strict_json_schema() {
        let sc = json!({ "type": "object" });
        let b = body("openai/gpt-4o", None, "hi", Some(&sc), false, None);
        assert_eq!(b["response_format"]["type"], json!("json_schema"));
        assert_eq!(b["response_format"]["json_schema"]["strict"], json!(true));
        assert_eq!(b["response_format"]["json_schema"]["schema"], sc);
    }

    /// The gateway reports what it charged, so a target routed through it is priced without a
    /// price-book entry — and a nonsense figure is dropped rather than published.
    #[test]
    fn a_reported_cost_is_used_and_a_nonsense_one_is_dropped() {
        assert_eq!(
            cost_usd(&json!({ "usage": { "cost": 0.0123 } })),
            Some(0.0123)
        );
        assert_eq!(cost_usd(&json!({ "usage": { "cost": 0.0 } })), Some(0.0));
        assert_eq!(cost_usd(&json!({ "usage": { "cost": -1.0 } })), None);
        assert_eq!(cost_usd(&json!({ "usage": {} })), None);
        assert_eq!(cost_usd(&json!({})), None);
    }

    /// The response shape is OpenAI's, so the truncation and reasoning readers are shared rather
    /// than re-implemented — a second copy is how two adapters come to disagree about one wire.
    #[test]
    fn the_openai_shaped_readers_are_reused() {
        let capped = json!({
            "choices": [{ "finish_reason": "length", "message": { "content": "" } }],
            "usage": { "completion_tokens": 4096, "completion_tokens_details": { "reasoning_tokens": 4096 } }
        });
        assert_eq!(
            super::super::openai::truncation(&capped),
            Some((4096, Some(4096)))
        );
        let ok = json!({ "completion_tokens": 900, "completion_tokens_details": { "reasoning_tokens": 700 } });
        assert_eq!(super::super::openai::reasoning_tokens(Some(&ok)), Some(700));
    }
}
