//! Candidate-output generation across providers. `anthropic` runs via `claude -p`; `google` and
//! `openai` call their HTTPS APIs (keys from env). Dollar cost is left `None` for the HTTP providers
//! (the caller prices it from the DB price book by tokens); the APIs don't return a cost.

use std::io::Read;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::{claude, EngineConfig, EngineError, GenOutcome, Result};

/// Outbound provider calls are bounded so a black-holed/overloaded endpoint can't hang an
/// (unbudgeted) benchmark worker forever, and a pathological body can't be buffered into memory.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard ceiling on a single provider response body (a completion is KBs; this stops a multi-GB body).
const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;

/// Process-wide blocking client, built once with bounded connect/request timeouts. reqwest pools and
/// reuses connections, so every provider call shares it.
fn http_client() -> Result<&'static reqwest::blocking::Client> {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| EngineError::Other(format!("http client init failed: {e}")))?;
    Ok(CLIENT.get_or_init(|| client))
}

/// Read a response body with a hard size cap, erroring out if the provider streams past it instead
/// of buffering an unbounded amount into memory.
fn read_bounded(resp: reqwest::blocking::Response, who: &str) -> Result<String> {
    let mut buf = Vec::new();
    resp.take(MAX_BODY_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| EngineError::Other(format!("{who} read failed: {e}")))?;
    if buf.len() as u64 > MAX_BODY_BYTES {
        return Err(EngineError::Other(format!(
            "{who} response exceeded {MAX_BODY_BYTES}-byte cap"
        )));
    }
    String::from_utf8(buf)
        .map_err(|e| EngineError::Other(format!("{who} returned non-UTF-8 body: {e}")))
}

/// Generate a candidate output from a target (provider + model + optional system-prompt variant).
pub fn generate(
    cfg: &EngineConfig,
    provider: &str,
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
) -> Result<GenOutcome> {
    match provider {
        "anthropic" => {
            let (envelope, latency_ms) = claude::invoke(cfg, input, model, system_prompt, None)?;
            let (input_tokens, output_tokens) = claude::token_counts(&envelope);
            Ok(GenOutcome {
                output: envelope
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                cost_usd: envelope.get("total_cost_usd").and_then(Value::as_f64),
                model: claude::model_of(&envelope, model),
                latency_ms,
                input_tokens,
                output_tokens,
            })
        }
        "google" => generate_gemini(model, system_prompt, input),
        "openai" => generate_openai(model, system_prompt, input),
        other => Err(EngineError::Other(format!("unknown provider '{other}'"))),
    }
}

/// Google Gemini `generateContent`. Key from GEMINI_API_KEY (or GOOGLE_* fallbacks).
fn generate_gemini(model: &str, system_prompt: Option<&str>, input: &str) -> Result<GenOutcome> {
    let key = std::env::var("GEMINI_API_KEY")
        .or_else(|_| std::env::var("GOOGLE_API_KEY"))
        .or_else(|_| std::env::var("GOOGLE_GENERATIVE_AI_API_KEY"))
        .map_err(|_| EngineError::Other("no Gemini API key (set GEMINI_API_KEY)".into()))?;
    let url =
        format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent");
    let mut body = serde_json::json!({ "contents": [{ "role": "user", "parts": [{ "text": input }] }] });
    if let Some(sys) = system_prompt {
        body["system_instruction"] = serde_json::json!({ "parts": [{ "text": sys }] });
    }

    let started = Instant::now();
    let resp = http_client()?
        .post(&url)
        .header("x-goog-api-key", &key)
        .json(&body)
        .send()
        .map_err(|e| EngineError::Other(format!("gemini request failed: {e}")))?;
    let latency_ms = Some(started.elapsed().as_millis() as u64);
    let status = resp.status();
    let text = read_bounded(resp, "gemini")?;
    if !status.is_success() {
        return Err(EngineError::Other(format!("gemini HTTP {}: {text}", status.as_u16())));
    }
    let v: Value = serde_json::from_str(&text)?;
    let output = v
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.get(0))
        .and_then(|p| p.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let usage = v.get("usageMetadata");
    Ok(GenOutcome {
        output,
        cost_usd: None,
        model: model.to_string(),
        latency_ms,
        input_tokens: usage.and_then(|u| u.get("promptTokenCount")).and_then(Value::as_u64),
        output_tokens: usage
            .and_then(|u| u.get("candidatesTokenCount"))
            .and_then(Value::as_u64),
    })
}

/// OpenAI Chat Completions. Key from OPENAI_API_KEY.
fn generate_openai(model: &str, system_prompt: Option<&str>, input: &str) -> Result<GenOutcome> {
    let key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| EngineError::Other("no OpenAI API key (set OPENAI_API_KEY)".into()))?;
    let mut messages = Vec::new();
    if let Some(sys) = system_prompt {
        messages.push(serde_json::json!({ "role": "system", "content": sys }));
    }
    messages.push(serde_json::json!({ "role": "user", "content": input }));
    let body = serde_json::json!({ "model": model, "messages": messages });

    let started = Instant::now();
    let resp = http_client()?
        .post("https://api.openai.com/v1/chat/completions")
        .bearer_auth(&key)
        .json(&body)
        .send()
        .map_err(|e| EngineError::Other(format!("openai request failed: {e}")))?;
    let latency_ms = Some(started.elapsed().as_millis() as u64);
    let status = resp.status();
    let text = read_bounded(resp, "openai")?;
    if !status.is_success() {
        return Err(EngineError::Other(format!("openai HTTP {}: {text}", status.as_u16())));
    }
    let v: Value = serde_json::from_str(&text)?;
    let output = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let usage = v.get("usage");
    Ok(GenOutcome {
        output,
        cost_usd: None,
        model: v
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| model.to_string()),
        latency_ms,
        input_tokens: usage.and_then(|u| u.get("prompt_tokens")).and_then(Value::as_u64),
        output_tokens: usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(Value::as_u64),
    })
}
