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
//!
//! Reasoning effort is `output_config.effort`, whose five levels are exactly ours, so a target
//! declaring `@xhigh` gets `xhigh` and nothing is collapsed or invented. It is deliberately *not*
//! `thinking: {type: "enabled", budget_tokens: N}`: that shape is rejected with a 400 by every model
//! [`resolve_model`] can produce, so it would be a request that never succeeds.

use std::time::Instant;

use serde_json::Value;

use lighttrack_core::{split_effort, Effort};

use crate::{Determinism, EngineError, GenOutcome, Result, SchemaEnforcement};

/// Env var that switches the `anthropic` provider onto this path.
pub(crate) const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
/// Pinned wire version — the Messages API requires it on every request.
const API_VERSION: &str = "2023-06-01";
/// Output ceiling for a judge verdict. A verdict is a small JSON object; this is generous headroom,
/// not a target.
const MAX_TOKENS: u64 = 4096;
/// Output ceiling once `effort` reaches `xhigh` or `max`. Anthropic's own migration guidance is
/// explicit that those two levels need `max_tokens >= 64000` or the response truncates mid-thought:
/// the cap governs thinking *and* answer, and at the top two levels the thinking alone can outrun
/// 4096. Still a ceiling, not a target — a verdict stays a small object — but a ceiling set where
/// the model can reach a normal stop instead of being cut off by ours.
const MAX_TOKENS_HIGH_EFFORT: u64 = 64_000;
/// The forced tool the judge answers through when a schema is supplied.
const VERDICT_TOOL: &str = "verdict";

/// True when the bare API path is available (a key is present in the environment).
pub(crate) fn available() -> bool {
    std::env::var(API_KEY_ENV).is_ok_and(|k| !k.trim().is_empty())
}

/// Resolve a CLI-style model alias to a Messages API model id. The `claude -p` aliases (`haiku`,
/// `sonnet`, `opus`) don't exist on the API, so a judge spec written for the CLI would 404 here.
/// Anything already looking like a model id passes through untouched.
///
/// Any `@effort` suffix is stripped **first**. The dispatch in `providers::generate_once` already
/// splits it, and this is the belt to that braces: `resolve_model` is the last thing standing
/// between a judge spec and `body["model"]`, and the default spec is `opus@xhigh` — the string that
/// used to arrive here whole, miss every alias arm, and be POSTed as a model id.
fn resolve_model(model: &str) -> &str {
    match split_effort(model).0 {
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
    effort: Option<Effort>,
) -> Result<GenOutcome> {
    let resolved = resolve_model(model);
    let key = std::env::var(API_KEY_ENV)
        .map_err(|_| EngineError::Other(format!("no Anthropic API key (set {API_KEY_ENV})")))?;

    match send(
        &key,
        resolved,
        model,
        system_prompt,
        input,
        schema,
        deterministic,
        effort,
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
            send(
                &key,
                resolved,
                model,
                system_prompt,
                input,
                schema,
                false,
                effort,
            )
        }
        other => other,
    }
}

/// The output ceiling for a call at `effort`. See [`MAX_TOKENS_HIGH_EFFORT`] — the top two levels
/// need real headroom or the model's own thinking hits *our* cap and the verdict is reported as
/// [`EngineError::Truncated`] when nothing was wrong with the model.
fn max_tokens_for(effort: Option<Effort>) -> u64 {
    match effort {
        Some(Effort::XHigh) | Some(Effort::Max) => MAX_TOKENS_HIGH_EFFORT,
        _ => MAX_TOKENS,
    }
}

/// The `POST /v1/messages` request body. `resolved` is a real Messages API model id — the caller
/// has already run it through [`resolve_model`], so no alias and no `@effort` suffix reaches
/// `body["model"]`.
///
/// Effort is `output_config.effort`, and our five levels ARE the API's five levels (`low` … `max`),
/// so the mapping is the identity — no invented rung, nothing collapsed. Note what this is *not*:
/// `thinking: {type: "enabled", budget_tokens: N}` is rejected with a 400 by every model
/// [`resolve_model`] produces (Opus 5 / Sonnet 5 / Haiku 4.5's siblings), so a token budget would
/// have been a request that never succeeds. `output_config` is GA and takes no beta header.
fn body(
    resolved: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
    deterministic: bool,
    effort: Option<Effort>,
) -> Value {
    let mut body = serde_json::json!({
        "model": resolved,
        "max_tokens": max_tokens_for(effort),
        "messages": [{ "role": "user", "content": input }],
    });
    if let Some(level) = effort {
        body["output_config"] = serde_json::json!({ "effort": level.as_str() });
    }
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
    body
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
    effort: Option<Effort>,
) -> Result<GenOutcome> {
    let body = body(
        resolved,
        system_prompt,
        input,
        schema,
        deterministic,
        effort,
    );

    let started = Instant::now();
    let resp = crate::providers::http_client()?
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", key)
        .header("anthropic-version", API_VERSION)
        // Per-call, because `max_tokens_for` just handed this request a 64k ceiling at the top two
        // effort levels and the shared client's 120s was measured against default-effort calls.
        .timeout(crate::providers::request_timeout(effort))
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
    let usage = v.get("usage");
    // Read the stop condition BEFORE the payload is interpreted: a body cut off by `max_tokens`
    // must never be salvaged into a fragment (empty or not) and read as the model's own doing.
    if let Some(cap) = truncation_cap(&v) {
        // The Messages API's `usage` carries no reasoning/answer split, so that field stays `None`
        // here — an honest gap, not a guess.
        return Err(EngineError::Truncated {
            who: "anthropic".into(),
            cap,
            reasoning_tokens: None,
        });
    }
    let output = completion_text(&v, schema.is_some());
    if output.is_empty() {
        return Err(EngineError::EmptyCompletion {
            who: "anthropic".into(),
        });
    }
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
        // `usage` has no thinking/answer split; thinking is inside `output_tokens`.
        reasoning_tokens: None,
        // Temperature-pinned, but Anthropic exposes no seed — reproducible by convention only.
        determinism: Determinism::BestEffort,
        schema: SchemaEnforcement::NotRequested,
    })
}

/// The token cap that applied, when a Messages API response was cut off by `stop_reason:
/// "max_tokens"` — read back from `usage.output_tokens`, which is exactly that cap by construction.
/// `None` on any other stop reason (including a response with no `stop_reason` at all).
fn truncation_cap(v: &Value) -> Option<u64> {
    if v.get("stop_reason").and_then(Value::as_str) != Some("max_tokens") {
        return None;
    }
    Some(
        v.pointer("/usage/output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    )
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

    /// **The defect this path shipped with.** `opus@xhigh` is the DEFAULT judge spec, and with a key
    /// present it came here whole: it matched no alias arm and was POSTed as `body["model"]`. An
    /// effort suffix must resolve to exactly what the bare alias resolves to.
    #[test]
    fn an_effort_suffixed_alias_resolves_like_the_bare_alias() {
        assert_eq!(resolve_model("opus@xhigh"), resolve_model("opus"));
        assert_eq!(resolve_model("haiku@low"), "claude-haiku-4-5");
        assert_eq!(
            resolve_model("claude-opus-5@max"),
            "claude-opus-5",
            "a real id keeps its identity once the level is off it"
        );
        // A model id that merely contains an `@` is not a spec — it survives whole.
        assert_eq!(resolve_model("weird@thing"), "weird@thing");
    }

    /// **The wire-body guarantee.** Whatever the caller wrote, `body["model"]` is a Messages API
    /// model id with no `@` in it.
    #[test]
    fn the_model_field_never_carries_an_effort_suffix() {
        let b = body(resolve_model("opus@xhigh"), None, "hi", None, false, None);
        assert_eq!(b["model"], json!("claude-opus-5"));
        assert!(
            !b["model"].as_str().unwrap().contains('@'),
            "{}",
            b["model"]
        );
        assert_eq!(b["max_tokens"], json!(MAX_TOKENS));
    }

    /// **The effort knob.** Our five levels are the API's five levels, so the body carries the level
    /// verbatim under `output_config.effort` — the GA parameter — and nothing else changes.
    #[test]
    fn effort_travels_as_output_config_effort() {
        for level in Effort::ALL {
            let b = body("claude-opus-5", None, "hi", None, false, Some(level));
            assert_eq!(
                b["output_config"]["effort"],
                json!(level.as_str()),
                "the level must reach the wire verbatim"
            );
            assert!(
                b.get("thinking").is_none(),
                "a budget_tokens thinking block is a 400 on every model this path resolves to"
            );
        }
        // No effort asked for: no output_config at all, so a stored matrix's request is unchanged.
        let plain = body("claude-opus-5", None, "hi", None, false, None);
        assert!(plain.get("output_config").is_none());
    }

    /// The top two levels get real output headroom: at `xhigh`/`max` the model's own thinking can
    /// outrun a 4096 ceiling, and a verdict cut off by OUR cap is reported as
    /// [`EngineError::Truncated`] — a cap of ours blamed on the model.
    #[test]
    fn the_top_two_levels_raise_the_output_ceiling() {
        assert_eq!(max_tokens_for(None), MAX_TOKENS);
        assert_eq!(max_tokens_for(Some(Effort::High)), MAX_TOKENS);
        assert_eq!(max_tokens_for(Some(Effort::XHigh)), MAX_TOKENS_HIGH_EFFORT);
        assert_eq!(max_tokens_for(Some(Effort::Max)), MAX_TOKENS_HIGH_EFFORT);
        let b = body("claude-opus-5", None, "hi", None, false, Some(Effort::Max));
        assert_eq!(b["max_tokens"], json!(MAX_TOKENS_HIGH_EFFORT));
    }

    /// Effort must not touch the determinism surface: `temperature: 0` is still requested, so the
    /// existing detect-and-retry (some models 400 on any sampling parameter) still fires and the
    /// outcome is still stamped [`Determinism::BestEffort`] — never upgraded because we now think
    /// harder.
    #[test]
    fn effort_leaves_the_determinism_request_alone() {
        let b = body("claude-opus-5", None, "hi", None, true, Some(Effort::XHigh));
        assert_eq!(b["temperature"], json!(0.0));
        assert_eq!(b["output_config"]["effort"], json!("xhigh"));
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

    /// `stop_reason: "max_tokens"` with an EMPTY `content` array — a forced tool call that never
    /// got far enough to emit its `tool_use` block. Distinct from a normal stop that said nothing:
    /// `truncation_cap` must say `Some`, not defer to `completion_text`'s empty string.
    #[test]
    fn max_tokens_with_empty_content_is_a_truncation_cap() {
        let resp = json!({
            "stop_reason": "max_tokens",
            "content": [],
            "usage": { "input_tokens": 200, "output_tokens": 4096 }
        });
        assert_eq!(truncation_cap(&resp), Some(4096));
        assert_eq!(completion_text(&resp, true), "", "no tool_use block landed");
    }

    /// `stop_reason: "max_tokens"` with a PARTIAL text block cut off mid-object — still a
    /// truncation, and the cap must be read before that fragment is ever handed to a JSON parser.
    #[test]
    fn max_tokens_with_partial_content_is_still_a_truncation_cap() {
        let resp = json!({
            "stop_reason": "max_tokens",
            "content": [{ "type": "text", "text": "{\"score\": 0.9, \"reasoning\": \"cut off mid" }],
            "usage": { "input_tokens": 200, "output_tokens": 4096 }
        });
        assert_eq!(truncation_cap(&resp), Some(4096));
    }

    /// A normal `end_turn` stop with empty content is NOT a truncation — `truncation_cap` says
    /// `None`, leaving `EmptyCompletion`'s "stopped normally, said nothing" meaning untouched.
    #[test]
    fn a_normal_stop_with_empty_content_is_not_a_truncation_cap() {
        let resp = json!({
            "stop_reason": "end_turn",
            "content": [],
            "usage": { "input_tokens": 200, "output_tokens": 0 }
        });
        assert_eq!(truncation_cap(&resp), None);
    }
}
