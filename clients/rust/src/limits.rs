//! Read what an ingest response says about the project's limits.
//!
//! Parsing only. What a client *does* with the answer — pausing before it spends, honouring the
//! schedule, shedding its own traffic — is pre-spend admission, and lives in [`crate::admission`].
//! Splitting the two matters because the reading is the part that must be identical in every SDK:
//! the same bytes have to mean the same thing in Python, TypeScript and Rust, or a fleet mixing them
//! enforces three different policies. The cases are fixed in `clients/contract/fixtures/limits.json`.
//!
//! The recurring trap this closes is `None` vs `0`. A project with no limits reports no ratio at
//! all; a client that read the absence as `0.0` would believe it had infinite headroom. An
//! unparseable `Retry-After` is likewise unknown, not "retry immediately".
//!
//! Signals arrive on two channels. `POST /v1/events` carries them as body fields. The batch door
//! answers multi-status (the project's position is not a property of item 7) and the OTLP door
//! answers in the exporter's own envelope, so neither has a body field to put them in — both send
//! `X-LightTrack-Usage-Ratio` / `-Shed-Fraction` / `-Retry-After` instead, and so does the 429,
//! which has no `IngestResponse` body at all. The body wins where both are present.

use serde_json::Value;

/// The dimension the binding rule applies to. `None` on a view means project-wide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingScope {
    /// `provider` | `model` | `name` | `api_key` | `customer`.
    pub kind: String,
    pub value: String,
}

/// What an ingest response says about limits. Every unknown is `None`, never a stand-in value.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitView {
    /// The event was recorded (2xx).
    pub accepted: bool,
    /// Refused for a usage limit (429) — a hard cap or graduated shedding.
    pub rate_limited: bool,
    /// Worst usage ratio among the rules that applied; `1.0` is at the cap. `None` when unknown.
    pub usage_ratio: Option<f64>,
    /// Share of ingest currently being shed, `0.0`–`1.0`. `None` when nothing is throttling.
    pub shed_fraction: Option<f64>,
    /// Seconds to wait, from `Retry-After`. `None` when absent or not a number (e.g. an HTTP-date).
    pub retry_after_secs: Option<u64>,
    /// The API's stable error code (`rate_limited`, `bad_request`, …). `None` on success.
    pub error_code: Option<String>,
    /// Which rule the ratio belongs to; `None` = project-wide (or unknown). `0.94` alone says stop
    /// everything, `0.94` on `model=gpt-4o` says route the next call elsewhere and keep working.
    pub binding_scope: Option<BindingScope>,
    /// Id of the binding rule. The server's shed decision is a hash of `(rule_id, event_id)`, so
    /// this is what lets a client reproduce it rather than merely run the same function.
    pub binding_rule: Option<String>,
}

/// Header lookup that does not care about casing — HTTP does not guarantee it, proxies rewrite it.
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn header_number(headers: &[(String, String)], name: &str) -> Option<f64> {
    header(headers, name)
        .map(str::trim)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

/// Integer seconds from a `Retry-After`-shaped header value.
fn retry_after_secs(raw: Option<&str>) -> Option<u64> {
    // Deliberately integer-only: `Retry-After` may also be an HTTP-date, and a half-parsed date that
    // came out as 0 would tell the client to hammer the endpoint it was just asked to back off from.
    raw.map(str::trim)
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u64>().ok())
}

/// Parse one ingest response into a [`LimitView`].
///
/// Pure and total: any shape of body, including none at all, yields a view rather than an error.
pub fn parse_limit_view(
    status: u16,
    headers: &[(String, String)],
    body: Option<&Value>,
) -> LimitView {
    let obj = body.filter(|b| b.is_object());
    // The standard header is the contract; the `X-LightTrack-` mirror is the copy that survives a
    // proxy which dropped the original. Never the other way round.
    let retry = retry_after_secs(header(headers, "retry-after"))
        .or_else(|| retry_after_secs(header(headers, "x-lighttrack-retry-after")));

    LimitView {
        accepted: (200..300).contains(&status),
        rate_limited: status == 429,
        usage_ratio: obj
            .and_then(|b| b["usage_ratio"].as_f64())
            .or_else(|| header_number(headers, "x-lighttrack-usage-ratio")),
        shed_fraction: obj
            .and_then(|b| b["shed_fraction"].as_f64())
            .or_else(|| header_number(headers, "x-lighttrack-shed-fraction")),
        retry_after_secs: retry,
        error_code: obj
            .and_then(|b| b["error"]["code"].as_str())
            .map(str::to_string),
        binding_scope: obj.and_then(|b| {
            let sc = &b["binding_scope"];
            match (sc["kind"].as_str(), sc["value"].as_str()) {
                (Some(kind), Some(value)) => Some(BindingScope {
                    kind: kind.to_string(),
                    value: value.to_string(),
                }),
                _ => None,
            }
        }),
        binding_rule: obj
            .and_then(|b| b["binding_rule"].as_str())
            .map(str::to_string),
    }
}
