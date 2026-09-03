//! The PII rule set `guard(no_pii)` runs — the server's own, not a copy of it.
//!
//! `crates/anon` exports its scrubbing rules to `clients/contract/fixtures/pii.json`; this module
//! embeds that file at compile time. Before it, the SDK carried a hand-written four-row table that
//! had drifted: it still ran the pre-D14 phone regex, which flags every ISO date and every dotted
//! version string as a phone number. A guard fronting an ingest path must not disagree with the
//! ingest path about what PII is, and the only way to guarantee that is to stop keeping a second
//! copy.
//!
//! `include_str!` rather than a runtime read: the fixture has to be present when the crate compiles,
//! and a missing or malformed one is a build-time failure instead of a surprise at the first
//! `guard` call.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

const FIXTURE: &str = include_str!("../../contract/fixtures/pii.json");

/// One scrubbing rule. Several rules may share a `kind`: a phone number has three shapes.
#[derive(Debug, Clone)]
pub struct PiiRule {
    /// Family name: `email`, `iban`, `ssn`, `secret`, `phone`, `credit_card`, `ip`.
    pub kind: String,
    /// Restricted to the RE2 / JS / Python / Rust common subset: no lookaround, no backreferences.
    pub pattern: String,
    pub placeholder: String,
}

/// The rule set, in evaluation order (most specific first).
pub fn rules() -> &'static [PiiRule] {
    static RULES: OnceLock<Vec<PiiRule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let doc: Value = serde_json::from_str(FIXTURE).expect("pii.json is valid JSON");
        doc["rules"]
            .as_array()
            .map(|rs| {
                rs.iter()
                    .filter_map(|r| {
                        Some(PiiRule {
                            kind: r["kind"].as_str()?.to_string(),
                            pattern: r["pattern"].as_str()?.to_string(),
                            placeholder: r["placeholder"].as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

fn compiled() -> &'static [(String, Regex)] {
    static COMPILED: OnceLock<Vec<(String, Regex)>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        rules()
            .iter()
            // A pattern that will not compile is dropped rather than panicking: telemetry must never
            // take the host app down. The anon export test is what makes this branch unreachable.
            .filter_map(|r| Regex::new(&r.pattern).ok().map(|re| (r.kind.clone(), re)))
            .collect()
    })
}

/// The PII families present in `text`, in rule order, each reported once.
///
/// A kind is a family, not a regex: a caller wants to know *a phone number leaked*, not which of
/// three patterns noticed.
pub fn pii_kinds(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for (kind, re) in compiled() {
        if !found.iter().any(|k| k == kind) && re.is_match(text) {
            found.push(kind.clone());
        }
    }
    found
}
