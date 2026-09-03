//! Generating a candidate from an **HTTP target** — an endpoint the operator owns, not a model.
//!
//! The gap this closes: LightTrack could only benchmark `{provider, model, system_prompt}`, so a
//! team whose quality lives in a RAG pipeline, a retrieval chain or an agent had nothing to
//! benchmark. Here the whole pipeline is the target: we POST the case and read the answer back.
//!
//! It lives beside [`providers`](crate::providers) rather than inside it because it is not a
//! provider — it belongs to no [`ProviderFamily`](lighttrack_core::ProviderFamily) we can name, and
//! the caller treats it as `Other` throughout: never the judge's family (see
//! [`BenchTarget::family_provider`](lighttrack_core::BenchTarget::family_provider)), and priced from
//! the `usage` the endpoint chose to report or not priced at all. Inventing a cost for an opaque
//! service would be worse than the existing unpriced path, which at least says it does not know.
//!
//! **Signature.** Every request carries `X-LightTrack-Signature: sha256=<hex>`, an HMAC-SHA256 over
//! the exact request body keyed by `LIGHTTRACK_HTTP_TARGET_SECRET`. A benchmark target URL is
//! operator-supplied and the endpoint is being asked to spend real work per case, so it needs a way
//! to tell our traffic from anyone who learned the URL. Absent the env var the header is omitted
//! (a purely local endpoint need not care) — it is never sent unsigned-but-present.

use std::time::Instant;

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::providers::{http_client, http_error, read_bounded, send_error};
use crate::retry::with_retry;
use crate::{Determinism, EngineError, GenOutcome, Result};

/// Env var holding the shared secret the request body is signed with.
pub const SECRET_ENV: &str = "LIGHTTRACK_HTTP_TARGET_SECRET";
/// Header carrying `sha256=<hex>` of the HMAC over the request body.
pub const SIGNATURE_HEADER: &str = "X-LightTrack-Signature";

type HmacSha256 = Hmac<Sha256>;

/// What we POST to an HTTP target. Deliberately the benchmark's own vocabulary — the case's
/// `input`, the reference answer when the dataset has one, and the resolved prompt — so an endpoint
/// can honour whichever parts it understands and ignore the rest.
#[derive(Debug, Clone, Serialize)]
pub struct HttpTargetRequest<'a> {
    pub input: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<&'a str>,
}

/// What we read back. Only `output` is required: an endpoint that reports token `usage` gets its
/// generation priced from the book like any other target, and one that does not is left honestly
/// unpriced rather than assigned a made-up number.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HttpTargetResponse {
    pub output: String,
    #[serde(default)]
    pub usage: Option<HttpTargetUsage>,
    #[serde(default)]
    pub latency_ms: Option<u64>,
    /// A dollar cost the endpoint knows and we cannot derive (it may call several models).
    #[serde(default)]
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct HttpTargetUsage {
    #[serde(default, alias = "prompt_tokens")]
    pub input_tokens: Option<u64>,
    #[serde(default, alias = "completion_tokens")]
    pub output_tokens: Option<u64>,
}

/// Lowercase hex, so the signature is comparable byte-for-byte on the other side.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// `sha256=<hex>` for `body` under `secret` — the value of [`SIGNATURE_HEADER`]. Public so a
/// receiving endpoint (and this crate's tests) can compute the same thing.
pub fn sign(secret: &str, body: &[u8]) -> Result<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|e| EngineError::Other(format!("http-target signing key: {e}")))?;
    mac.update(body);
    Ok(format!("sha256={}", hex(&mac.finalize().into_bytes())))
}

/// POST one case to an HTTP target and read its answer.
///
/// Retried under the same transient-failure policy as every provider call, so a 429/5xx/timeout
/// from someone's staging pipeline does not lose a paid benchmark case. `Determinism` is
/// [`BestEffort`](Determinism::BestEffort): the endpoint exposes no sampling knobs to pin, and
/// claiming `Exact` for a black box would make an irreproducible run read as reproducible.
pub fn generate_http(
    url: &str,
    system_prompt: Option<&str>,
    input: &str,
    expected: Option<&str>,
) -> Result<GenOutcome> {
    let body = serde_json::to_vec(&HttpTargetRequest {
        input,
        expected,
        system_prompt,
    })?;
    let signature = match std::env::var(SECRET_ENV) {
        Ok(s) if !s.is_empty() => Some(sign(&s, &body)?),
        _ => None,
    };
    with_retry(|| call_once(url, &body, signature.as_deref()))
}

fn call_once(url: &str, body: &[u8], signature: Option<&str>) -> Result<GenOutcome> {
    let who = format!("http-target {url}");
    let started = Instant::now();
    let mut req = http_client()?
        .post(url)
        .header("content-type", "application/json")
        .body(body.to_vec());
    if let Some(sig) = signature {
        req = req.header(SIGNATURE_HEADER, sig);
    }
    let resp = req.send().map_err(|e| send_error(&who, e))?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let text = read_bounded(resp, &who)?;
    if !status.is_success() {
        return Err(http_error(&who, status, &headers, text));
    }
    let parsed: HttpTargetResponse = serde_json::from_str(&text).map_err(|e| {
        EngineError::Parse(format!(
            "{who} returned a body this build cannot read (expected {{\"output\": \"…\"}}): {e}"
        ))
    })?;
    if parsed.output.is_empty() {
        return Err(EngineError::EmptyCompletion { who });
    }
    let usage = parsed.usage.unwrap_or_default();
    Ok(GenOutcome {
        output: parsed.output,
        cost_usd: parsed.cost_usd,
        model: url.to_string(),
        // Prefer the endpoint's own figure — it knows what it spent inside; ours includes our
        // network hop. Fall back to the measured wall clock so latency is never simply missing.
        latency_ms: parsed
            .latency_ms
            .or_else(|| Some(started.elapsed().as_millis() as u64)),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        determinism: Determinism::BestEffort,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signature_is_hmac_sha256_over_the_exact_body() {
        // Pinned against an independently-known vector so a future refactor of `sign` cannot
        // silently change the bytes every receiving endpoint verifies against.
        let sig = sign("key", b"The quick brown fox jumps over the lazy dog").unwrap();
        assert_eq!(
            sig,
            "sha256=f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
        // A different body — or a different key — is a different signature.
        assert_ne!(sign("key", b"other").unwrap(), sig);
        assert_ne!(
            sign("other-key", b"The quick brown fox jumps over the lazy dog").unwrap(),
            sig
        );
    }

    #[test]
    fn the_request_body_omits_what_the_case_does_not_have() {
        let body = serde_json::to_value(HttpTargetRequest {
            input: "hi",
            expected: None,
            system_prompt: None,
        })
        .unwrap();
        assert_eq!(body, serde_json::json!({ "input": "hi" }));
    }

    #[test]
    fn a_response_without_usage_reads_as_unpriced_not_as_zero() {
        let r: HttpTargetResponse = serde_json::from_str(r#"{"output":"answer"}"#).unwrap();
        assert!(r.usage.is_none(), "no usage is absence, not 0 tokens");
        assert!(r.cost_usd.is_none());
        // OpenAI-style usage keys are accepted too, since that is what most services already emit.
        let r: HttpTargetResponse = serde_json::from_str(
            r#"{"output":"a","usage":{"prompt_tokens":7,"completion_tokens":3}}"#,
        )
        .unwrap();
        let u = r.usage.unwrap();
        assert_eq!((u.input_tokens, u.output_tokens), (Some(7), Some(3)));
    }
}
