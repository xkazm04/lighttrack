//! The Google Gemini `generateContent` adapter.
//!
//! Split out of `providers.rs` when the effort axis arrived: the request body is now built by a pure
//! function so a test can assert what actually goes on the wire — the model, which travels in the
//! *path* here rather than the body, and the `generationConfig` block.

use std::time::Instant;

use serde_json::Value;

use lighttrack_core::Effort;

use super::{
    api_base, effort_unsupported, http_client, http_error, read_bounded, schema_state, send_error,
    PINNED_SEED,
};
use crate::{Determinism, EngineError, GenOutcome, Result};

/// Recursively drop a JSON-schema key the provider's schema subset doesn't accept (Gemini's
/// `responseSchema` rejects `additionalProperties`).
fn strip_schema_key(v: &Value, key: &str) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| k.as_str() != key)
                .map(|(k, val)| (k.clone(), strip_schema_key(val, key)))
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().map(|i| strip_schema_key(i, key)).collect())
        }
        other => other.clone(),
    }
}

/// **The gap, stated rather than guessed.** Gemini's thinking control is
/// `generationConfig.thinkingConfig.thinkingBudget` — a *token count*, not a named level, whose
/// valid range differs per model (and where `-1` means "let the model decide"). Turning `high` into
/// a number therefore requires a per-model table this build does not have and could not verify from
/// the code, and a budget that a model silently clamps or ignores produces the worst outcome
/// available: a leaderboard column that reads as measured and measures nothing.
///
/// So the plumbing reaches here and stops here, loudly. Closing this gap means a verified
/// level → budget table per Gemini model, not a constant.
const GEMINI_EFFORT_GAP: &str = "Gemini's thinking control is \
     `generationConfig.thinkingConfig.thinkingBudget`, a token count whose valid range is per-model, \
     and this build has no verified level-to-budget table — a guessed budget the API clamps or \
     ignores would produce a scorecard column that measures nothing";

/// The `generateContent` request body.
fn body(
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
    deterministic: bool,
    effort: Option<Effort>,
) -> Result<Value> {
    if let Some(level) = effort {
        return Err(effort_unsupported(
            "gemini",
            model,
            level,
            GEMINI_EFFORT_GAP,
        ));
    }
    let mut body =
        serde_json::json!({ "contents": [{ "role": "user", "parts": [{ "text": input }] }] });
    if let Some(sys) = system_prompt {
        body["system_instruction"] = serde_json::json!({ "parts": [{ "text": sys }] });
    }
    let mut gen_config = serde_json::Map::new();
    if let Some(sc) = schema {
        gen_config.insert(
            "responseMimeType".into(),
            serde_json::json!("application/json"),
        );
        gen_config.insert(
            "responseSchema".into(),
            strip_schema_key(sc, "additionalProperties"),
        );
    }
    if deterministic {
        gen_config.insert("temperature".into(), serde_json::json!(0.0));
        gen_config.insert("seed".into(), serde_json::json!(PINNED_SEED));
    }
    if !gen_config.is_empty() {
        body["generationConfig"] = Value::Object(gen_config);
    }
    Ok(body)
}

/// The `generateContent` URL. The model rides in the **path** here, so the `@effort` split matters
/// just as much as it does for a body field — an unsplit spec would be requested as a model id.
fn url(model: &str) -> String {
    format!(
        "{}/v1beta/models/{model}:generateContent",
        api_base(
            "LIGHTTRACK_GEMINI_BASE",
            "https://generativelanguage.googleapis.com"
        )
    )
}

/// Google Gemini `generateContent`. Key from GEMINI_API_KEY (or GOOGLE_* fallbacks).
pub(super) fn generate(
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
    deterministic: bool,
    effort: Option<Effort>,
) -> Result<GenOutcome> {
    let key = std::env::var("GEMINI_API_KEY")
        .or_else(|_| std::env::var("GOOGLE_API_KEY"))
        .or_else(|_| std::env::var("GOOGLE_GENERATIVE_AI_API_KEY"))
        .map_err(|_| EngineError::Other("no Gemini API key (set GEMINI_API_KEY)".into()))?;
    let body = body(model, system_prompt, input, schema, deterministic, effort)?;

    let started = Instant::now();
    let resp = http_client()?
        .post(url(model))
        .header("x-goog-api-key", &key)
        .json(&body)
        .send()
        .map_err(|e| send_error("gemini", e))?;
    let latency_ms = Some(started.elapsed().as_millis() as u64);
    let status = resp.status();
    // Cloned BEFORE the body is read: `read_bounded` consumes the response, and the stated retry
    // schedule lives in the headers it takes with it.
    let headers = resp.headers().clone();
    let text = read_bounded(resp, "gemini")?;
    if !status.is_success() {
        return Err(http_error("gemini", status, &headers, text));
    }
    let v: Value = serde_json::from_str(&text)?;
    let usage = v.get("usageMetadata");
    // Read the stop condition BEFORE the payload is interpreted: a thinking model that spends its
    // whole cap on `thought` parts reaches `text` as 0 characters, which must read as a cap hit,
    // not as a normal stop that said nothing.
    if let Some((cap, reasoning_tokens)) = truncation(&v) {
        return Err(EngineError::Truncated {
            who: "gemini".into(),
            cap,
            reasoning_tokens,
        });
    }
    let output = text_of(&v);
    if output.is_empty() {
        return Err(EngineError::EmptyCompletion {
            who: "gemini".into(),
        });
    }
    Ok(GenOutcome {
        output,
        cost_usd: None,
        model: model.to_string(),
        latency_ms,
        input_tokens: usage
            .and_then(|u| u.get("promptTokenCount"))
            .and_then(Value::as_u64),
        output_tokens: billed_output_tokens(usage),
        reasoning_tokens: usage
            .and_then(|u| u.get("thoughtsTokenCount"))
            .and_then(Value::as_u64),
        // temperature 0 + a fixed seed were both accepted: reproducible by contract.
        schema: schema_state(schema),
        determinism: if deterministic {
            Determinism::Exact
        } else {
            Determinism::BestEffort
        },
    })
}

/// The output tokens Gemini **bills**: the answer (`candidatesTokenCount`) plus the hidden reasoning
/// (`thoughtsTokenCount`). The first alone is the visible answer only, and a thinking model's thoughts
/// are charged at the output rate — so pricing a call from the book on `candidatesTokenCount` priced
/// a 40-token answer that thought for 1,500 as a 40-token call. `None` only when neither is reported.
pub(super) fn billed_output_tokens(usage: Option<&Value>) -> Option<u64> {
    let u = usage?;
    let answer = u.get("candidatesTokenCount").and_then(Value::as_u64);
    let thoughts = u.get("thoughtsTokenCount").and_then(Value::as_u64);
    if answer.is_none() && thoughts.is_none() {
        return None;
    }
    Some(answer.unwrap_or(0) + thoughts.unwrap_or(0))
}

/// Whether a `generateContent` response was cut off by `maxOutputTokens`, and if so, the cap that
/// applied plus the tokens spent on hidden reasoning (`thoughtsTokenCount`), when the response
/// reports one. `cap` is the answer tokens plus the reasoning tokens: the cap governs the whole
/// generation, not just the visible part, and a thinking model can spend all of it before emitting
/// a single answer token.
pub(super) fn truncation(v: &Value) -> Option<(u64, Option<u64>)> {
    if v.pointer("/candidates/0/finishReason")
        .and_then(Value::as_str)
        != Some("MAX_TOKENS")
    {
        return None;
    }
    let usage = v.get("usageMetadata");
    let answer_tokens = usage
        .and_then(|u| u.get("candidatesTokenCount"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning_tokens = usage
        .and_then(|u| u.get("thoughtsTokenCount"))
        .and_then(Value::as_u64);
    Some((
        answer_tokens + reasoning_tokens.unwrap_or(0),
        reasoning_tokens,
    ))
}

/// The answer text of a `generateContent` response: every text part of the first candidate,
/// skipping thought parts. The reader used to take `parts[0].text` only, and a thinking model puts
/// its `thought: true` part first — so its every verdict read as an empty completion, and a
/// multi-part answer lost everything after the first part.
pub(super) fn text_of(v: &Value) -> String {
    v.pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|p| !p.get("thought").and_then(Value::as_bool).unwrap_or(false))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .concat()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// **The wire guarantee for a path-carried model.** `gemini-2.5-pro@low` reaches this adapter
    /// already split, so the request URL names a model that exists.
    #[test]
    fn the_model_in_the_path_never_carries_an_effort_suffix() {
        let u = url("gemini-2.5-pro");
        assert!(
            u.ends_with("/v1beta/models/gemini-2.5-pro:generateContent"),
            "{u}"
        );
        assert!(
            !u.contains('@'),
            "no @-suffixed model may reach the wire: {u}"
        );
    }

    /// **The stated absence.** Every level errors on this adapter, and the message names the gap —
    /// a guessed `thinkingBudget` the API clamps or ignores is worse than a refusal, because it
    /// yields a scorecard column that reads as measured and measures nothing.
    #[test]
    fn every_effort_errors_and_the_message_names_the_missing_mapping() {
        for level in Effort::ALL {
            let err = body("gemini-2.5-pro", None, "hi", None, false, Some(level)).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("gemini"), "names the adapter: {msg}");
            assert!(msg.contains("gemini-2.5-pro"), "names the model: {msg}");
            assert!(msg.contains(level.as_str()), "names the level: {msg}");
            assert!(msg.contains("thinkingBudget"), "names the gap: {msg}");
        }
    }

    #[test]
    fn deterministic_pins_temperature_and_seed_in_the_generation_config() {
        let b = body("gemini-2.5-pro", Some("terse"), "hi", None, true, None).unwrap();
        assert_eq!(b["generationConfig"]["temperature"], json!(0.0));
        assert_eq!(b["generationConfig"]["seed"], json!(PINNED_SEED));
        assert_eq!(b["system_instruction"]["parts"][0]["text"], json!("terse"));
        // Without a schema or pinning there is no generationConfig at all — unchanged behaviour.
        let plain = body("gemini-2.5-pro", None, "hi", None, false, None).unwrap();
        assert!(plain.get("generationConfig").is_none());
    }

    /// A thinking model's first part is its thought; the verdict is the text part after it.
    #[test]
    fn the_answer_skips_thought_parts_and_joins_the_rest() {
        let thinking = json!({ "candidates": [{ "content": { "parts": [
            { "thought": true, "text": "let me think" },
            { "text": "{\"score\":" }, { "text": "0.5}" }
        ] } }] });
        assert_eq!(text_of(&thinking), "{\"score\":0.5}");
        let plain = json!({ "candidates": [{ "content": { "parts": [{ "text": "hi" }] } }] });
        assert_eq!(text_of(&plain), "hi");
        assert_eq!(text_of(&json!({ "candidates": [] })), "");
        assert_eq!(text_of(&json!({})), "");
    }

    /// `MAX_TOKENS` with an EMPTY answer is truncation, not an empty completion.
    #[test]
    fn a_capped_call_with_zero_answer_tokens_is_truncated_not_empty() {
        let v = json!({
            "candidates": [{ "finishReason": "MAX_TOKENS", "content": { "parts": [] } }],
            "usageMetadata": { "candidatesTokenCount": 0, "thoughtsTokenCount": 16000 }
        });
        assert_eq!(truncation(&v), Some((16000, Some(16000))));
    }

    /// `MAX_TOKENS` with a PARTIAL answer is still truncation, reported before the fragment is seen.
    #[test]
    fn a_capped_call_with_a_partial_answer_is_still_truncated() {
        let v = json!({
            "candidates": [{ "finishReason": "MAX_TOKENS", "content": { "parts": [{ "text": "{\"score\": 0.8" }] } }],
            "usageMetadata": { "candidatesTokenCount": 16000 }
        });
        assert_eq!(truncation(&v), Some((16000, None)));
    }

    /// **The undercount.** A thinking model's billed output is its answer plus its thoughts; the
    /// book priced only the answer, so a call that thought for 1,500 tokens cost what 40 would.
    #[test]
    fn billed_output_includes_the_thoughts_a_thinking_model_spent() {
        let thinking = json!({ "candidatesTokenCount": 40, "thoughtsTokenCount": 1500 });
        assert_eq!(billed_output_tokens(Some(&thinking)), Some(1540));
        // A non-thinking response is unchanged.
        let plain = json!({ "candidatesTokenCount": 40 });
        assert_eq!(billed_output_tokens(Some(&plain)), Some(40));
        // Thoughts with no answer count (a capped-out thinker) still bill.
        assert_eq!(
            billed_output_tokens(Some(&json!({ "thoughtsTokenCount": 900 }))),
            Some(900)
        );
        // Unreported is unknown, never zero.
        assert_eq!(billed_output_tokens(Some(&json!({}))), None);
        assert_eq!(billed_output_tokens(None), None);
    }

    /// A normal stop with an empty answer is NOT truncation.
    #[test]
    fn a_normal_stop_with_an_empty_answer_is_not_truncation() {
        let v = json!({
            "candidates": [{ "finishReason": "STOP", "content": { "parts": [] } }],
            "usageMetadata": { "candidatesTokenCount": 0 }
        });
        assert_eq!(truncation(&v), None);
    }

    #[test]
    fn strips_additional_properties_recursively() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "dim": { "type": "object", "additionalProperties": false, "properties": { "score": { "type": "number" } } }
            }
        });
        let cleaned = strip_schema_key(&schema, "additionalProperties");
        assert!(cleaned.get("additionalProperties").is_none());
        assert!(cleaned["properties"]["dim"]
            .get("additionalProperties")
            .is_none());
        // Untouched keys survive.
        assert_eq!(
            cleaned["properties"]["dim"]["properties"]["score"]["type"],
            "number"
        );
    }
}
