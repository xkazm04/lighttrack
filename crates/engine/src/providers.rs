//! Candidate-output generation across providers. `anthropic` runs via `claude -p` (or the bare
//! Messages API when a key is present); `google` and `openai` call their HTTPS APIs (keys from env).
//! Dollar cost is left `None` for the HTTP providers (the caller prices it from the DB price book by
//! tokens); the APIs don't return a cost.
//!
//! Structured output is enforced when a `schema` is supplied: `--json-schema` for the claude CLI,
//! `response_format:{type:"json_schema",…}` for OpenAI, and `generationConfig.responseSchema` (+ JSON
//! MIME type) for Gemini. Transient failures (429/5xx/timeout) are retried with backoff; a provider
//! that *rejects* the schema (4xx) falls back once to a schema-less prose call so a strict-schema
//! model never hard-fails a run.
//!
//! Layout: this file owns the shared HTTP plumbing (client, error classification, bounded reads) and
//! the dispatch; the per-provider request bodies live in `providers/openai.rs` and
//! `providers/gemini.rs`, beside the tests that assert what goes on their wire.

mod gemini;
mod openai;

use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

use crate::invocation::{self, Invocation};
use crate::retry::with_retry;
use lighttrack_core::{split_effort, Effort, ProviderFamily};

use crate::{
    anthropic_api, Determinism, EngineConfig, EngineError, GenOutcome, Result, SchemaEnforcement,
};

/// Outbound provider calls are bounded so a black-holed/overloaded endpoint can't hang an
/// (unbudgeted) benchmark worker forever, and a pathological body can't be buffered into memory.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Long enough for a reasoning model to finish a rubric verdict. At 30s a thinking model that
/// takes 45s timed out three times in a row — 90s spent, the sample lost, and the retry policy
/// working exactly as designed against a call that was never going to fit.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
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

/// Stated-delay headers, in precedence order, with the multiplier that turns each one's units into
/// milliseconds. First match wins. Two unit systems live here: the `-ms` spellings are milliseconds
/// (OpenAI and Azure OpenAI both emit them), plain `Retry-After` is seconds (RFC 9110).
const RETRY_AFTER_HEADERS: [(&str, u64); 3] = [
    ("retry-after-ms", 1),
    ("x-ms-retry-after-ms", 1),
    ("retry-after", 1000),
];

/// The delay the provider **stated** on this response, if any.
///
/// Classification happens once, here, at the boundary that still holds the structured response —
/// the retry loop consumes the typed class and never re-parses anything. Only the delay-seconds
/// form is read; `Retry-After` may also carry an HTTP-date, which needs a date parser we do not
/// have a dependency for, and misreading one as a number would be worse than falling back to the
/// computed ladder (which is what a `None` here does).
pub(crate) fn stated_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    for (name, to_millis) in RETRY_AFTER_HEADERS {
        let Some(raw) = headers.get(name).and_then(|v| v.to_str().ok()) else {
            continue;
        };
        // Deliberately unclamped: a provider naming an implausibly long wait is not corrected here,
        // it is *reported* — the ladder's budget rule turns it into a distinct terminal state
        // carrying the number, and truncating it here would delete that evidence.
        match raw.trim().parse::<f64>() {
            Ok(n) if n.is_finite() && n >= 0.0 => {
                return Some(Duration::from_millis((n * to_millis as f64) as u64))
            }
            _ => continue,
        }
    }
    None
}

/// Map an HTTP status + headers + body to a typed error (retryability is decided by the variant,
/// never by string-matching the message; a rate limit additionally carries the provider's own
/// stated schedule).
pub(crate) fn http_error(
    who: &str,
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: String,
) -> EngineError {
    let s = status.as_u16();
    match s {
        429 => EngineError::RateLimited {
            who: who.to_string(),
            retry_after: stated_retry_after(headers),
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

/// The error an adapter returns when it is handed an effort level it has no wire parameter for.
///
/// Loud on purpose. The alternative — dropping the level and calling the provider anyway — is how a
/// scorecard column ends up labelled `@xhigh` while measuring the model's default thinking, which is
/// exactly the failure the `@effort` suffix produced for a year: parsed in one of four generation
/// paths, ignored by the other three.
pub(crate) fn effort_unsupported(who: &str, model: &str, effort: Effort) -> EngineError {
    EngineError::Other(format!(
        "the {who} adapter cannot request effort '{effort}' for model '{model}': this build has no \
         mapping from an effort level to a {who} request parameter, and silently dropping it would \
         report a default-effort run as an '{effort}' one"
    ))
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
            generate_retrying(cfg, provider, model, system_prompt, input, None, false).map(
                |mut o| {
                    // The caller asked for a schema and is not getting one. Say so in the value, not
                    // only on stderr: downstream parses the output believing syntax was enforced.
                    o.schema = SchemaEnforcement::Shed;
                    o
                },
            )
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
    // **The `@effort` suffix dies here**, at the one point every generation path passes through.
    // It used to be split inside the `claude -p` argv builder only, so with `ANTHROPIC_API_KEY` set
    // the default judge spec `opus@xhigh` was POSTed to the Messages API as a *model id*; Gemini and
    // OpenAI had the same hole for any suffixed spec. Below this line `model` is a real model id and
    // the level travels as a typed [`Effort`] the adapter must either honour or refuse.
    let (model, effort) = split_effort(model);

    // Route on the provider's **family**, not its literal id: a judge spec may name any provider
    // (M8), and `azure-openai` / `az.ai.openai` are OpenAI endpoints in every way that matters here.
    // A provider we cannot classify gets a message that says what is missing — an adapter — rather
    // than "unknown provider", which reads as "you typed it wrong".
    match lighttrack_core::family_of(provider) {
        // Prefer the bare Messages API when a key is present: no ~40k-token auto-loaded CLI context
        // (DECISIONS D9) and `temperature: 0` is at least askable. Without a key the only way in is
        // the CLI's subscription OAuth, and that path has no sampling knobs at all.
        ProviderFamily::Anthropic if anthropic_api::available() => {
            anthropic_api::generate(model, system_prompt, input, schema, deterministic, effort)
        }
        // The Claude CLI has no sampling knobs to pass; the deterministic request is best-effort.
        ProviderFamily::Anthropic => {
            generate_anthropic(cfg, model, system_prompt, input, schema, effort)
        }
        ProviderFamily::Google => {
            gemini::generate(model, system_prompt, input, schema, deterministic, effort)
        }
        ProviderFamily::OpenAi => {
            openai::generate(model, system_prompt, input, schema, deterministic, effort)
        }
        other => Err(EngineError::Other(format!(
            "no generation adapter for provider '{provider}' (family {other}); this build can \
             generate with anthropic, google and openai endpoints only — observability and pricing \
             accept any provider, generation does not"
        ))),
    }
}

/// The schema guarantee a provider call *asked for*. A caller that supplied no schema gets
/// `NotRequested`; one that supplied a schema the provider accepted gets `Enforced`. The third
/// state, `Shed`, is not knowable here — it is stamped by [`generate`], which owns the
/// reject-and-retry-schema-less fallback and is the only place that knows the retry happened.
fn schema_state(schema: Option<&Value>) -> SchemaEnforcement {
    if schema.is_some() {
        SchemaEnforcement::Enforced
    } else {
        SchemaEnforcement::NotRequested
    }
}

/// The API origin for a provider, overridable by env. Two callers need this: the provider-boundary
/// suite, which points the *real* call path at a local stub rather than mocking the path away, and
/// anyone routing these calls through a gateway. Empty is treated as unset.
pub(crate) fn api_base(var: &str, default: &str) -> String {
    std::env::var(var)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Fixed seed for pinned calls (judge *and* candidate generation) — any constant works; what
/// matters is that it never varies, so two runs of the same benchmark ask for the same draw.
pub const PINNED_SEED: u64 = 42;

/// Anthropic via `claude -p`, passing the schema through `--json-schema` (serialized). This is the
/// one adapter that has always honoured effort — `--effort <level>` — and it now receives the level
/// from the dispatch rather than re-parsing the model spec itself.
fn generate_anthropic(
    cfg: &EngineConfig,
    model: &str,
    system_prompt: Option<&str>,
    input: &str,
    schema: Option<&Value>,
    effort: Option<Effort>,
) -> Result<GenOutcome> {
    let schema_str = schema.map(|s| s.to_string());
    let out = invocation::run(
        &cfg.claude(),
        &Invocation::generate(input, model)
            .with_system(system_prompt)
            .with_schema(schema_str.as_deref())
            .with_effort(effort.map(|e| e.as_str()))
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
        schema: schema_state(schema),
    })
}

#[cfg(test)]
mod tests {
    use super::{effort_unsupported, schema_state, stated_retry_after};
    use crate::SchemaEnforcement;
    use lighttrack_core::Effort;
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    use std::time::Duration;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    /// Three spellings, two unit systems, first match wins — and a header we cannot read is a
    /// `None` that falls back to the computed ladder, never a misread number.
    #[test]
    fn reads_the_stated_delay_in_precedence_order() {
        assert_eq!(
            stated_retry_after(&headers(&[("retry-after", "5")])),
            Some(Duration::from_secs(5)),
            "plain Retry-After is SECONDS"
        );
        assert_eq!(
            stated_retry_after(&headers(&[("retry-after-ms", "250")])),
            Some(Duration::from_millis(250)),
            "the -ms spellings are MILLISECONDS"
        );
        // The ms spelling is more precise, so it outranks the seconds one when both are present.
        assert_eq!(
            stated_retry_after(&headers(&[("retry-after", "5"), ("retry-after-ms", "80")])),
            Some(Duration::from_millis(80))
        );
        assert_eq!(
            stated_retry_after(&headers(&[("x-ms-retry-after-ms", "120")])),
            Some(Duration::from_millis(120))
        );
        // An HTTP-date Retry-After is not guessed at.
        assert_eq!(
            stated_retry_after(&headers(&[(
                "retry-after",
                "Wed, 21 Oct 2026 07:28:00 GMT"
            )])),
            None
        );
        assert_eq!(stated_retry_after(&HeaderMap::new()), None);
        assert_eq!(stated_retry_after(&headers(&[("retry-after", "-3")])), None);
    }

    /// A caller must be able to tell an enforced schema from a prose fallback **from the value it
    /// holds**. Before `GenOutcome::schema` existed, the reject-and-retry-schema-less path in
    /// [`super::generate`] reported the degradation on stderr only, so these three cases were
    /// indistinguishable downstream and the third was parsed as though syntax were guaranteed.
    #[test]
    fn schema_state_distinguishes_requested_from_absent() {
        let sc = serde_json::json!({"type": "object"});
        assert_eq!(schema_state(Some(&sc)), SchemaEnforcement::Enforced);
        assert_eq!(schema_state(None), SchemaEnforcement::NotRequested);
        // The shed state is not derivable from the request alone — it is stamped by `generate`
        // after a provider rejection, and it must not collapse into either of the other two.
        assert_ne!(SchemaEnforcement::Shed, SchemaEnforcement::Enforced);
        assert_ne!(SchemaEnforcement::Shed, SchemaEnforcement::NotRequested);
        assert_eq!(SchemaEnforcement::Shed.as_str(), "shed");
    }

    /// The refusal names all three things an operator needs to act: which adapter, which model,
    /// which level. A message that said only "unsupported" would leave them re-reading the matrix.
    #[test]
    fn an_unsupported_effort_names_the_adapter_model_and_level() {
        let msg = effort_unsupported("gemini", "gemini-2.5-pro", Effort::XHigh).to_string();
        assert!(msg.contains("gemini"), "{msg}");
        assert!(msg.contains("gemini-2.5-pro"), "{msg}");
        assert!(msg.contains("xhigh"), "{msg}");
    }
}
