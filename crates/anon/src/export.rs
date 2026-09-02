//! Export the scrubber's rule set to `clients/contract/fixtures/pii.json`, and fail when the
//! checked-in file has drifted from it.
//!
//! Why this test exists. The three client SDKs each carried their own copy of a four-row PII table
//! for `guard(no_pii)`. Byte-identical when written, and then the server's table moved: D14 replaced
//! `(?:\+?\d[\s().-]?){10,}` — which matches every ISO date and every dotted version string — with
//! shape-specific phone rules. The SDK copies did not move, so a client-side guard and the ingest
//! scrubber could disagree about what counts as PII, and nothing anywhere said so.
//!
//! The fix is a direction of flow, not more copies: the server owns the rules, this test renders
//! them, and every SDK reads the rendered file. A rule added here that nobody re-exports turns this
//! test red; an SDK table that drifts from the file turns *its* contract test red.
//!
//! Regenerate with `LIGHTTRACK_UPDATE_FIXTURES=1 cargo test -p lighttrack-anon`.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::rule_set;

/// Repo-relative home of the exported fixture.
const FIXTURE_REL: &str = "clients/contract/fixtures/pii.json";

fn fixture_path() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<repo>/crates/anon`.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join(FIXTURE_REL)
}

/// The document the SDKs consume. `rules` is ordered; the order is the evaluation order.
fn render() -> Value {
    json!({
        "$schema": "../schema.json",
        "generated_by": "crates/anon/src/export.rs (cargo test -p lighttrack-anon)",
        "source": "lighttrack_anon::rule_set()",
        "note": "Do not hand-edit. Change crates/anon/src/lib.rs and re-run the export test.",
        "rules": rule_set()
            .into_iter()
            .map(|r| json!({ "kind": r.kind, "pattern": r.pattern, "placeholder": r.placeholder }))
            .collect::<Vec<_>>(),
    })
}

fn serialize(doc: &Value) -> String {
    let mut s = serde_json::to_string_pretty(doc).expect("the rule set serializes");
    s.push('\n');
    s
}

#[test]
fn exported_pii_fixture_is_current() {
    let path = fixture_path();
    let rendered = serialize(&render());

    if std::env::var("LIGHTTRACK_UPDATE_FIXTURES").is_ok() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("fixture directory");
        }
        std::fs::write(&path, &rendered).expect("write the fixture");
        return;
    }

    let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{FIXTURE_REL} is missing ({e}). Regenerate it with \
             `LIGHTTRACK_UPDATE_FIXTURES=1 cargo test -p lighttrack-anon`."
        )
    });

    // Compare parsed, then raw: a semantic diff is the actionable message, formatting is the tail.
    let want: Value = serde_json::from_str(&rendered).expect("rendered json");
    let got: Value = serde_json::from_str(&on_disk)
        .unwrap_or_else(|e| panic!("{FIXTURE_REL} is not valid JSON: {e}"));
    assert_eq!(
        got["rules"], want["rules"],
        "{FIXTURE_REL} has drifted from lighttrack_anon::rule_set(). The scrubber is the source of \
         truth; regenerate with `LIGHTTRACK_UPDATE_FIXTURES=1 cargo test -p lighttrack-anon` and \
         re-run the SDK contract suites."
    );
    assert_eq!(
        on_disk, rendered,
        "{FIXTURE_REL} is semantically current but formatted differently; regenerate it with \
         `LIGHTTRACK_UPDATE_FIXTURES=1 cargo test -p lighttrack-anon`."
    );
}

/// Every exported pattern must be portable. The SDKs run these in JavaScript's `RegExp`, Python's
/// `re` and Rust's `regex` — only `regex` refuses lookaround and backreferences outright, so a
/// pattern that quietly relies on them would compile in two of the three and behave differently in
/// the third. Reject the constructs here, where one message explains it.
#[test]
fn exported_patterns_are_in_the_common_regex_subset() {
    for rule in rule_set() {
        let p = rule.pattern;
        regex::Regex::new(p)
            .unwrap_or_else(|e| panic!("rule '{}' does not compile in Rust regex: {e}", rule.kind));
        for bad in ["(?=", "(?!", "(?<=", "(?<!"] {
            assert!(
                !p.contains(bad),
                "rule '{}' uses lookaround `{bad}` in `{p}`: RE2 and Rust's regex reject it, so the \
                 SDK guards could not run the same rule the scrubber does",
                rule.kind
            );
        }
        // A backreference is `\1`..`\9`. Character classes and escapes never legitimately produce
        // a backslash followed by a digit in this table, so the check is exact rather than a guess.
        let bytes = p.as_bytes();
        for i in 0..bytes.len().saturating_sub(1) {
            if bytes[i] == b'\\' && bytes[i + 1].is_ascii_digit() && bytes[i + 1] != b'0' {
                panic!(
                    "rule '{}' looks like it uses a backreference in `{p}`: not portable to RE2/Rust",
                    rule.kind
                );
            }
        }
        assert!(
            !rule.kind.is_empty() && !rule.placeholder.is_empty(),
            "every rule needs a kind and a placeholder"
        );
    }
}

/// The kinds are the SDK-visible names. Freezing the set here means renaming one is a deliberate
/// contract change (this test, then every SDK's `guard` expectations) rather than a silent one.
#[test]
fn exported_kinds_are_the_documented_families() {
    let mut kinds: Vec<&str> = rule_set().into_iter().map(|r| r.kind).collect();
    kinds.sort_unstable();
    kinds.dedup();
    assert_eq!(
        kinds,
        vec![
            "credit_card",
            "email",
            "iban",
            "ip",
            "phone",
            "secret",
            "ssn"
        ],
        "the PII kind vocabulary changed; update clients/contract/schema.json and the SDK guards"
    );
}
