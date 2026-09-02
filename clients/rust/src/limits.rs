//! Read what an ingest response says about the project's limits.
//!
//! Parsing only. What a client *does* with the answer — pausing before it spends, honouring the
//! schedule, shedding its own traffic — is pre-spend admission, and lives elsewhere. Splitting the
//! two matters because the reading is the part that must be identical in every SDK: the same bytes
//! have to mean the same thing in Python, TypeScript and Rust, or a fleet mixing them enforces three
//! different policies. The cases are fixed in `clients/contract/fixtures/limits.json`.
//!
//! The recurring trap this closes is `None` vs `0`. A project with no limits reports no ratio at
//! all; a client that read the absence as `0.0` would believe it had infinite headroom. An
//! unparseable `Retry-After` is likewise unknown, not "retry immediately".

use serde_json::Value;

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
}

/// Header lookup that does not care about casing — HTTP does not guarantee it, proxies rewrite it.
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Parse one ingest response into a [`LimitView`].
///
/// Pure and total: any shape of body, including none at all, yields a view rather than an error.
pub fn parse_limit_view(status: u16, headers: &[(String, String)], body: Option<&Value>) -> LimitView {
    let obj = body.filter(|b| b.is_object());
    // Deliberately integer-only: `Retry-After` may also be an HTTP-date, and a half-parsed date that
    // came out as 0 would tell the client to hammer the endpoint it was just asked to back off from.
    let retry = header(headers, "retry-after")
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u64>().ok());

    LimitView {
        accepted: (200..300).contains(&status),
        rate_limited: status == 429,
        usage_ratio: obj.and_then(|b| b["usage_ratio"].as_f64()),
        shed_fraction: obj.and_then(|b| b["shed_fraction"].as_f64()),
        retry_after_secs: retry,
        error_code: obj
            .and_then(|b| b["error"]["code"].as_str())
            .map(str::to_string),
    }
}
