//! The enforcement half of pre-spend admission: what the client does with a refusal.
//!
//! The verdicts themselves are cross-language and live in the contract suite. What is language-local
//! — and what this file pins — is that the cache is fed from a real ingest response, that the wait a
//! 429 advertises is honoured and then expires, and that "unknown" admits.

use lighttrack_client::{parse_limit_view, AdmissionCache, AdmitReason};
use serde_json::json;

fn observe(
    cache: &mut AdmissionCache,
    status: u16,
    headers: &[(&str, &str)],
    body: serde_json::Value,
    at: i64,
) {
    let hs: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let b = (!body.is_null()).then_some(body);
    cache.observe(&parse_limit_view(status, &hs, b.as_ref()), at);
}

#[test]
fn an_unobserved_cache_admits() {
    // Fail open: "unknown" must never read as "over budget", or installing LightTrack is an outage.
    let cache = AdmissionCache::default();
    assert!(cache.admit(None, None, 0).ok);
}

#[test]
fn the_advertised_wait_is_honoured_then_expires() {
    let mut cache = AdmissionCache::new(60_000);
    observe(
        &mut cache,
        429,
        &[("Retry-After", "30")],
        json!({ "error": { "code": "rate_limited", "message": "over cap" } }),
        0,
    );
    let refused = cache.admit(None, None, 5_000);
    assert!(!refused.ok);
    assert_eq!(refused.reason, Some(AdmitReason::RetryAfter));
    assert_eq!(refused.retry_after_secs, Some(25));
    // A back-off that never lifts is a broken client.
    assert!(cache.admit(None, None, 31_000).ok);
}

#[test]
fn the_signal_is_read_from_headers_when_the_body_has_none() {
    // The batch and OTLP doors answer in shapes that cannot hold the field, so a client reading only
    // the body would be blind on two of the three ingest doors.
    let mut cache = AdmissionCache::new(60_000);
    observe(
        &mut cache,
        200,
        &[("X-LightTrack-Usage-Ratio", "1.000000")],
        json!({ "accepted": 1 }),
        0,
    );
    let v = cache.admit(None, None, 1_000);
    assert!(!v.ok);
    assert_eq!(v.reason, Some(AdmitReason::AtCap));
    assert_eq!(v.retry_after_secs, None, "no schedule was ever advertised");
}

#[test]
fn a_scoped_cap_stops_only_its_own_call_site() {
    let mut cache = AdmissionCache::new(60_000);
    observe(
        &mut cache,
        200,
        &[],
        json!({
            "usage_ratio": 1.0,
            "binding_scope": { "kind": "name", "value": "summarize" },
            "binding_rule": "rule-sum"
        }),
        0,
    );
    assert!(!cache.admit(Some("summarize"), None, 1_000).ok);
    // Applying the worst rule in the project to every call is how a scoped budget becomes an outage.
    assert!(cache.admit(Some("translate"), None, 1_000).ok);
}

#[test]
fn a_stale_view_admits_and_says_so() {
    let mut cache = AdmissionCache::new(30_000);
    observe(&mut cache, 200, &[], json!({ "usage_ratio": 1.0 }), 0);
    let v = cache.admit(None, None, 60_000);
    assert!(v.ok, "past the TTL the numbers are no longer evidence");
    assert!(v.stale, "and the caller has to be told, so it can refresh");
}
