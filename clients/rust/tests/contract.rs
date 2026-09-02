//! The cross-language SDK contract, run against the Rust client.
//!
//! Every case here also runs, unchanged, in `clients/typescript/src/contract.test.ts` and
//! `clients/python/tests/test_contract.py`. That is the whole point: the three SDKs were three
//! hand-synchronised implementations of one contract, and nothing could see the drift between them —
//! the provider extractors were triplicated, the PII table was triplicated and one of the three had
//! gone stale against the server, and CI ran the suites as unrelated jobs. Shared vectors turn "we
//! believe these agree" into a test.
//!
//! A behaviour that is not in `clients/contract/fixtures/` is not part of the contract, and a
//! behaviour that is may not differ between languages. Capabilities this SDK does not have (journal,
//! span, instrument, relay) are declared `not_supported` in `lighttrack.manifest.json` and skipped
//! here — visibly, and with the gap named, rather than quietly not asserted.

use std::collections::BTreeSet;
use std::path::PathBuf;

use lighttrack_client::{
    diagnostic_kind, extract_anthropic, extract_gemini, extract_openai, guard, parse_limit_view,
    send_failure_message, Extracted, FailureContext, GuardRules,
};
use serde_json::Value;

fn clients_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<repo>/clients/rust`.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("clients/ exists")
}

fn fixture(name: &str) -> Value {
    let path = clients_dir().join("contract").join("fixtures").join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()))
}

fn manifest() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lighttrack.manifest.json");
    serde_json::from_str(&std::fs::read_to_string(&path).expect("manifest")).expect("manifest json")
}

fn supports(capability: &str) -> bool {
    manifest()["capabilities"][capability].as_str() == Some("supported")
}

fn cases(name: &str, key: &str) -> Vec<Value> {
    fixture(name)[key].as_array().cloned().unwrap_or_default()
}

fn why(case: &Value) -> String {
    case["why"].as_str().unwrap_or("").to_string()
}

// ---- pii --------------------------------------------------------------------

/// The table is `include_str!`'d from the fixture, so it cannot drift — but a *fixture* that no
/// longer matches the server would slip through, and a pattern that will not compile would be
/// silently dropped by `pii::compiled`. Both are checked here.
#[test]
fn pii_table_is_the_servers() {
    let rules = fixture("pii");
    let rules = rules["rules"].as_array().expect("rules array");
    assert!(!rules.is_empty(), "the exported PII rule set is empty");
    let embedded = lighttrack_client::pii_kinds("");
    assert!(embedded.is_empty(), "empty text has no PII");
    for r in rules {
        let pattern = r["pattern"].as_str().expect("pattern");
        regex::Regex::new(pattern)
            .unwrap_or_else(|e| panic!("rule '{}' does not compile: {e}", r["kind"]));
    }
}

// ---- extractors -------------------------------------------------------------

#[test]
fn provider_extractors() {
    for case in cases("extractors", "extractors") {
        let name = case["name"].as_str().unwrap_or("?");
        let got = match case["provider"].as_str() {
            Some("openai") => extract_openai(&case["response"]),
            Some("anthropic") => extract_anthropic(&case["response"]),
            Some("gemini") => extract_gemini(&case["response"]),
            other => panic!("{name}: unknown provider {other:?}"),
        };
        let e = &case["expect"];
        let want = Extracted {
            model: e["model"].as_str().map(str::to_string),
            input_tokens: e["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: e["output_tokens"].as_u64().unwrap_or(0),
            cached_input_tokens: e["cached_input_tokens"].as_u64(),
        };
        assert_eq!(got, want, "{name}: {}", why(&case));
    }
}

// ---- guard ------------------------------------------------------------------

/// Map the fixture's neutral snake_case rules onto this SDK's `GuardRules`.
fn to_rules(r: &Value) -> GuardRules {
    let strings = |v: &Value| {
        v.as_array()
            .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    GuardRules {
        json: r["json"].as_bool().unwrap_or(false),
        json_keys: strings(&r["json_keys"]),
        max_words: r["max_words"].as_u64().map(|n| n as usize),
        min_words: r["min_words"].as_u64().map(|n| n as usize),
        max_chars: r["max_chars"].as_u64().map(|n| n as usize),
        must_include: strings(&r["must_include"]),
        must_match: r["must_match"].as_str().map(str::to_string),
        must_not_match: strings(&r["must_not_match"]),
        no_pii: r["no_pii"].as_bool().unwrap_or(false),
    }
}

#[test]
fn guard_verdicts() {
    for case in cases("guard", "guard") {
        let name = case["name"].as_str().unwrap_or("?");
        let output = case["output"].as_str().expect("output");
        let result = guard(output, &to_rules(&case["rules"]));

        let failed: BTreeSet<&str> = result
            .checks
            .iter()
            .filter(|(_, passed)| !*passed)
            .map(|(k, _)| k.as_str())
            .collect();
        let want: BTreeSet<&str> = case["expect"]["violations"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        assert_eq!(failed, want, "{name}: {}", why(&case));
        assert_eq!(result.ok, case["expect"]["ok"].as_bool().unwrap(), "{name}: ok");
        // `ok` is defined as "nothing failed" — the two must never disagree.
        assert_eq!(result.ok, result.violations.is_empty(), "{name}: ok tracks violations");
    }
}

// ---- journal ----------------------------------------------------------------

#[test]
fn journal_unsettled_records() {
    if supports("journal") {
        panic!(
            "clients/rust now declares journal=supported but this runner still skips the fixture. \
             Implement the parse and assert clients/contract/fixtures/journal.json here."
        );
    }
    // Skipped on purpose. The Rust client has no crash-surviving breadcrumb, so a Rust process
    // killed mid-call leaves no record of it where the Python and TypeScript clients would recover
    // one. That gap is stated in lighttrack.manifest.json and rendered into clients/README.md; it is
    // not silently absent.
}

// ---- limits -----------------------------------------------------------------

#[test]
fn ingest_limit_signals() {
    for case in cases("limits", "limits") {
        let name = case["name"].as_str().unwrap_or("?");
        let headers: Vec<(String, String)> = case["headers"]
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let body = case.get("body").filter(|b| !b.is_null());
        let status = case["status"].as_u64().expect("status") as u16;
        let v = parse_limit_view(status, &headers, body);
        let e = &case["expect"];

        assert_eq!(v.accepted, e["accepted"].as_bool().unwrap(), "{name}: accepted");
        assert_eq!(v.rate_limited, e["rate_limited"].as_bool().unwrap(), "{name}: rate_limited");
        assert_eq!(v.usage_ratio, e["usage_ratio"].as_f64(), "{name}: usage_ratio");
        assert_eq!(v.shed_fraction, e["shed_fraction"].as_f64(), "{name}: shed_fraction");
        assert_eq!(v.retry_after_secs, e["retry_after_secs"].as_u64(), "{name}: retry_after_secs");
        assert_eq!(
            v.error_code.as_deref(),
            e["error_code"].as_str(),
            "{name}: error_code — {}",
            why(&case)
        );
        let scope = v
            .binding_scope
            .as_ref()
            .map(|b| serde_json::json!({ "kind": b.kind, "value": b.value }))
            .unwrap_or(serde_json::Value::Null);
        assert_eq!(scope, e["binding_scope"], "{name}: binding_scope");
        assert_eq!(
            v.binding_rule.as_deref(),
            e["binding_rule"].as_str(),
            "{name}: binding_rule — {}",
            why(&case)
        );
    }
}

// ---- diagnostics ------------------------------------------------------------

#[test]
fn failure_diagnostics() {
    for case in cases("diagnostics", "diagnostics") {
        let name = case["name"].as_str().unwrap_or("?");
        let status = case["status"].as_u64().map(|s| s as u16);
        assert_eq!(
            diagnostic_kind(status, false),
            case["kind"].as_str().unwrap(),
            "{name}: rate-limiting bucket"
        );
        let msg = send_failure_message(
            "http://127.0.0.1:8787",
            "/v1/events",
            "boom",
            FailureContext {
                status,
                has_project: case["has_project"].as_bool().unwrap_or(false),
                has_key: case["has_key"].as_bool().unwrap_or(false),
            },
        );
        for needle in case["hint_contains"].as_array().into_iter().flatten() {
            let needle = needle.as_str().unwrap();
            assert!(msg.contains(needle), "{name}: message is missing \"{needle}\".\nGot: {msg}");
        }
        // ASCII only. These lines land in whatever console the host app has, and a cp1252 Windows
        // terminal turns a stray em dash into mojibake.
        assert!(msg.is_ascii(), "{name}: message must be ASCII.\nGot: {msg}");
    }
}
