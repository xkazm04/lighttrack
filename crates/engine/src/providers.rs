//! Candidate-output generation across providers. `anthropic` runs via `claude -p`; `google` and
//! `openai` call their HTTPS APIs (keys from env). Dollar cost is left `None` for the HTTP providers
//! (the caller prices it from the DB price book by tokens); the APIs don't return a cost.
//!
//! Structured output is enforced when a `schema` is supplied: `--json-schema` for the claude CLI,
//! `response_format:{type:"json_schema",…}` for OpenAI, and `generationConfig.responseSchema` (+ JSON
//! MIME type) for Gemini. Transient failures (429/5xx/timeout) are retried with backoff; a provider
//! that *rejects* the schema (4xx) falls back once to a schema-less prose call so a strict-schema
//! model never hard-fails a run.

use std::io::Read;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::invocation::{self, Invocation};
use crate::retry::with_retry;
use lighttrack_core::ProviderFamily;

use crate::{anthropic_api, Determinism, EngineConfig, EngineError, GenOutcome, Result};

/// Outbound provider calls are bounded so a black-holed/overloaded endpoint can't hang an
/// (unbudgeted) benchmark worker forever, and a pathological body can't be buffered into memory.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard ceiling on a single provider response body (a completion is KBs; this stops a multi-GB body).
const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;

/// Process-wide blocking client, built once with bounded connect/request timeouts. reqwest pools and
/// reuses connections, so every provider call shares it.
pub(crate) fn http_client() -> Result<&'static reqwest::blocking::Client> {
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

/// Map an HTTP status + body to a typed error (retryability is decided by the variant, never by
/// string-matching the message).
pub(crate) fn http_error(who: &str, status: reqwest::StatusCode, body: String) -> EngineError {
    let s = status.as_u16();
    match s {
        429 => EngineError::RateLimited {
            who: who.to_string(),
        },
        401 | 403 => EngineError::Auth {
            who: who.to_string(),
            status: s,
        },
        500..=599 => EngineError::ServerError {
            who: who.to_string(),
            status: s,
        },
        _ => EngineError::BadRequest {
            who: who.to_string(),
            status: s,
            body,
        },
    }
}

/// Map a reqwest transport error to a typed error: timeouts/connect failures are retryable.
pub(crate) fn send_error(who: &str, e: reqwest::Error) -> EngineError {
    if e.is_timeout() || e.is_connect() {
        EngineError::Timeout {
            who: who.to_string(),
        }
    } else {
        EngineError::Http {
            who: who.to_string(),
            detail: e.to_string(),
        }
    }
}

/// Read a response body with a hard size cap, erroring out if the provider streams past it instead
/// of buffering an unbounded amount into memory.
pub(crate) fn read_bounded(resp: reqwest::blocking::Response, who: &str) -> Result<String> {
    let mut buf = Vec::new();
    resp.take(MAX_BODY_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| EngineError::Http {
            who: who.to_string(),
            detail: e.to_string(),
        })?;
    if buf.len() as u64 > MAX_BODY_BYTES {
        return Err(EngineError::Other(format!(
            "{who} response exceeded {MAX_BODY_BYTES}-byte cap"
        )));
    }
    String::from_utf8(buf).map_err(|e| EngineError::Http {
        who: who.to_string(),
        detail: format!("non-UTF-8 body: {e}"),
    })
}

/// Generate a candidate output from a target (provider + model + optional system-prompt variant).
/// When `schema` is set, structured output is enforced; a provider that *rejects* the schema (a 4xx)
/// is retried once schema-less (a logged prose fallback) so strict-schema models never hard-fail.
pub fn generate(
    cfg: &EngineConfig,
    provider: &str,
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
) -> Result<GenOutcome> {
    match generate_retrying(cfg, provider, model, system_prompt, input, schema, false) {
        Err(EngineError::BadRequest { who, status, body }) if schema.is_some() => {
            eprintln!(
                "[judge] {who} rejected the JSON schema (HTTP {status}: {}); retrying schema-less",
                body.chars().take(200).collect::<String>()
            );
            generate_retrying(cfg, provider, model, system_prompt, input, None, false)
        }
        other => other,
    }
}

/// [`generate`] with **deterministic sampling requested** — used for both halves of a benchmark:
/// the judge call *and* the candidate generation it grades. A verdict should be a measurement, not
/// a sample: without `temperature: 0` (+ a fixed `seed` where the API takes one) the same rubric
/// over the same candidate can flip between runs, which both undermines reproducibility ("re-run
/// the eval, get the ranking you published") and confounds the self-consistency agreement metric
/// (disagreement should signal a genuinely ambiguous case, not sampling noise).
///
/// Pinning the judge alone was never enough: a freshly-sampled candidate makes the *whole* run
/// irreproducible however deterministic the grading of it was. So compare/pairwise pin generation
/// here too — except when the operator explicitly asked for several candidate draws, where the
/// variation is the measurement (stamped [`Determinism::Sampled`] by the caller).
///
/// What each provider actually gives us — recorded on the outcome as
/// [`Determinism`](crate::Determinism), not assumed:
/// - **OpenAI / Gemini** — `temperature: 0` + the fixed [`PINNED_SEED`] ⇒ `exact`.
/// - **Anthropic with `ANTHROPIC_API_KEY`** — the bare Messages API with `temperature: 0`; the API
///   exposes no `seed`, so `best-effort`. Still strictly better than the CLI: no ~40k-token
///   auto-loaded context and temperature is pinned.
/// - **Anthropic without a key** — `claude -p`, which exposes no sampling knobs at all ⇒
///   `best-effort`, and the residual the API path exists to shrink.
///
/// A provider that rejects either strict feature (schema or the sampling params — some reasoning
/// models refuse `temperature`) falls back once to a plain, non-deterministic schema-less call with
/// a loud log, so judging degrades rather than hard-fails.
pub fn generate_deterministic(
    cfg: &EngineConfig,
    provider: &str,
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
) -> Result<GenOutcome> {
    match generate_retrying(cfg, provider, model, system_prompt, input, schema, true) {
        Err(EngineError::BadRequest { who, status, body }) => {
            eprintln!(
                "[engine] {who} rejected the pinned request (HTTP {status}: {}); retrying \
                 schema-less and non-deterministic",
                body.chars().take(200).collect::<String>()
            );
            generate_retrying(cfg, provider, model, system_prompt, input, None, false)
        }
        other => other,
    }
}

/// One dispatch under the transient-failure retry policy.
fn generate_retrying(
    cfg: &EngineConfig,
    provider: &str,
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
    deterministic: bool,
) -> Result<GenOutcome> {
    with_retry(|| {
        generate_once(
            cfg,
            provider,
            model,
            system_prompt,
            input,
            schema,
            deterministic,
        )
    })
}

fn generate_once(
    cfg: &EngineConfig,
    provider: &str,
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
    deterministic: bool,
) -> Result<GenOutcome> {
    // Route on the provider's **family**, not its literal id: a judge spec may name any provider
    // (M8), and `azure-openai` / `az.ai.openai` are OpenAI endpoints in every way that matters here.
    // A provider we cannot classify gets a message that says what is missing — an adapter — rather
    // than "unknown provider", which reads as "you typed it wrong".
    match lighttrack_core::family_of(provider) {
        // Prefer the bare Messages API when a key is present: no ~40k-token auto-loaded CLI context
        // (DECISIONS D9) and `temperature: 0` is at least askable. Without a key the only way in is
        // the CLI's subscription OAuth, and that path has no sampling knobs at all.
        ProviderFamily::Anthropic if anthropic_api::available() => {
            anthropic_api::generate(model, system_prompt, input, schema, deterministic)
        }
        // The Claude CLI has no sampling knobs to pass; the deterministic request is best-effort.
        ProviderFamily::Anthropic => generate_anthropic(cfg, model, system_prompt, input, schema),
        ProviderFamily::Google => {
            generate_gemini(model, system_prompt, input, schema, deterministic)
        }
        ProviderFamily::OpenAi => {
            generate_openai(model, system_prompt, input, schema, deterministic)
        }
        other => Err(EngineError::Other(format!(
            "no generation adapter for provider '{provider}' (family {other}); this build can \
             generate with anthropic, google and openai endpoints only — observability and pricing \
             accept any provider, generation does not"
        ))),
    }
}

/// Fixed seed for pinned calls (judge *and* candidate generation) — any constant works; what
/// matters is that it never varies, so two runs of the same benchmark ask for the same draw.
pub const PINNED_SEED: u64 = 42;

/// Anthropic via `claude -p`, passing the schema through `--json-schema` (serialized).
fn generate_anthropic(
    cfg: &EngineConfig,
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
) -> Result<GenOutcome> {
    let schema_str = schema.map(|s| s.to_string());
    let out = invocation::run(
        &cfg.claude(),
        &Invocation::generate(input, model)
            .with_system(system_prompt)
            .with_schema(schema_str.as_deref())
            .with_bare(cfg.bare),
    )?;
    if out.text.is_empty() {
        return Err(EngineError::EmptyCompletion {
            who: "claude".into(),
        });
    }
    Ok(GenOutcome {
        output: out.text,
        cost_usd: out.cost_usd,
        model: out.model,
        latency_ms: out.latency_ms,
        input_tokens: out.input_tokens,
        output_tokens: out.output_tokens,
        // The CLI exposes neither temperature nor seed — this is the residual the bare API path
        // exists to shrink, and it is now stamped on the outcome instead of living in a comment.
        determinism: Determinism::BestEffort,
    })
}

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

/// Google Gemini `generateContent`. Key from GEMINI_API_KEY (or GOOGLE_* fallbacks).
fn generate_gemini(
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
    deterministic: bool,
) -> Result<GenOutcome> {
    let key = std::env::var("GEMINI_API_KEY")
        .or_else(|_| std::env::var("GOOGLE_API_KEY"))
        .or_else(|_| std::env::var("GOOGLE_GENERATIVE_AI_API_KEY"))
        .map_err(|_| EngineError::Other("no Gemini API key (set GEMINI_API_KEY)".into()))?;
    let url =
        format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent");
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

    let started = Instant::now();
    let resp = http_client()?
        .post(&url)
        .header("x-goog-api-key", &key)
        .json(&body)
        .send()
        .map_err(|e| send_error("gemini", e))?;
    let latency_ms = Some(started.elapsed().as_millis() as u64);
    let status = resp.status();
    let text = read_bounded(resp, "gemini")?;
    if !status.is_success() {
        return Err(http_error("gemini", status, text));
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
    if output.is_empty() {
        return Err(EngineError::EmptyCompletion {
            who: "gemini".into(),
        });
    }
    let usage = v.get("usageMetadata");
    Ok(GenOutcome {
        output,
        cost_usd: None,
        model: model.to_string(),
        latency_ms,
        input_tokens: usage
            .and_then(|u| u.get("promptTokenCount"))
            .and_then(Value::as_u64),
        output_tokens: usage
            .and_then(|u| u.get("candidatesTokenCount"))
            .and_then(Value::as_u64),
        // temperature 0 + a fixed seed were both accepted: reproducible by contract.
        determinism: if deterministic {
            Determinism::Exact
        } else {
            Determinism::BestEffort
        },
    })
}

/// OpenAI Chat Completions. Key from OPENAI_API_KEY.
fn generate_openai(
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
    deterministic: bool,
) -> Result<GenOutcome> {
    let key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| EngineError::Other("no OpenAI API key (set OPENAI_API_KEY)".into()))?;
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

    let started = Instant::now();
    let resp = http_client()?
        .post("https://api.openai.com/v1/chat/completions")
        .bearer_auth(&key)
        .json(&body)
        .send()
        .map_err(|e| send_error("openai", e))?;
    let latency_ms = Some(started.elapsed().as_millis() as u64);
    let status = resp.status();
    let text = read_bounded(resp, "openai")?;
    if !status.is_success() {
        return Err(http_error("openai", status, text));
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
    if output.is_empty() {
        return Err(EngineError::EmptyCompletion {
            who: "openai".into(),
        });
    }
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
        input_tokens: usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(Value::as_u64),
        output_tokens: usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(Value::as_u64),
        determinism: if deterministic {
            Determinism::Exact
        } else {
            Determinism::BestEffort
        },
    })
}

#[cfg(test)]
mod tests {
    use super::strip_schema_key;
    use serde_json::json;

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
