//! Heuristic PII scrubbing for dataset building.
//!
//! This is the **regex pass** of the hybrid anonymization pipeline (see
//! `docs/BENCHMARK_FRAMEWORK.md` §1): structured PII with reliable shapes — emails, IBANs, national
//! IDs, secrets, card numbers, IPs, phone numbers — replaced with typed placeholders. Free-text PII
//! (names, orgs, locations) is left to the optional LLM pass in the runner.
//!
//! Rules run in a fixed order (most specific → least) so e.g. an IP isn't eaten by the phone rule.
//!
//! **Precision is the point.** Scrubbed text is what gets stored, and the stored text is what the
//! LLM judge later reads — so an over-broad rule does not merely lose a date, it silently rewrites
//! the evidence downstream scoring is computed from, and nothing in a score, alert or dashboard ever
//! reveals it. Where a shape is ambiguous these rules deliberately **under-match**: a redaction we
//! miss is visible to whoever reads the row, a sentence we mangle is not.

use std::sync::OnceLock;

use regex::Regex;

/// Result of scrubbing: the cleaned text and how many spans were redacted.
#[derive(Debug, Clone)]
pub struct ScrubResult {
    pub text: String,
    pub redactions: usize,
}

pub(crate) struct Rule {
    kind: &'static str,
    pub(crate) re: Regex,
    pub(crate) placeholder: &'static str,
}

/// One scrubbing rule, flattened for export.
///
/// This is the shape the client SDKs consume from `clients/contract/fixtures/pii.json`: the
/// SDK-side `guard(no_pii)` used to carry its own hand-copied four-row table, which drifted (it
/// still ran the pre-D14 phone regex that eats ISO dates). Exporting the rule set makes the server
/// the single source and the fixture the wire between them — see `export.rs` for the stale-check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiiRule {
    /// Stable family name (`email`, `phone`, `credit_card`, ...). Several rules may share one kind:
    /// a phone number has three shapes and a secret three prefixes, but a caller only cares that
    /// *a phone* was found.
    pub kind: &'static str,
    pub pattern: &'static str,
    pub placeholder: &'static str,
}

/// The rule set, in evaluation order (most specific -> least). Order is part of the contract — it is
/// what keeps an IP address from being eaten by the phone rule.
pub fn rule_set() -> Vec<PiiRule> {
    rules()
        .iter()
        .map(|r| PiiRule {
            kind: r.kind,
            pattern: r.re.as_str(),
            placeholder: r.placeholder,
        })
        .collect()
}

pub(crate) fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let r = |k: &'static str, p: &str, ph: &'static str| Rule {
            kind: k,
            re: Regex::new(p).expect("valid regex"),
            placeholder: ph,
        };
        vec![
            r(
                "email",
                r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}",
                "<EMAIL>",
            ),
            r("iban", r"\b[A-Z]{2}\d{2}[A-Z0-9]{10,30}\b", "<IBAN>"),
            r("ssn", r"\b\d{3}-\d{2}-\d{4}\b", "<SSN>"),
            r("secret", r"\bsk-[A-Za-z0-9_\-]{16,}\b", "<SECRET>"),
            r("secret", r"\bAKIA[0-9A-Z]{12,}\b", "<SECRET>"),
            r("secret", r"\b[0-9a-fA-F]{32,}\b", "<SECRET>"),
            // A leading `+` is a phone signal no card number carries, so the two E.164 shapes are
            // tried before the card rule — otherwise a 13+ digit international number types as <CC>.
            r(
                "phone",
                r"\+\d{1,3}(?:[ \-](?:\(\d{1,4}\)|\d{2,4})){2,5}\b",
                "<PHONE>",
            ),
            r("phone", r"\+\d{1,3}[ \-]?\d{7,14}\b", "<PHONE>"),
            // Anchored on a digit at *both* ends: the old `(?:\d[ \-]?){13,19}` let the last
            // repetition swallow its trailing separator, so `card 4111 1111 1111 1111 was` came back
            // as `card <CC>was`.
            r("credit_card", r"\b\d(?:[ \-]?\d){12,18}\b", "<CC>"),
            // Real octets only. `\d{1,3}` also claimed impossible quads (999.999.999.999), which is
            // pure corruption — nothing rejected here was ever an address.
            r(
                "ip",
                r"\b(?:25[0-5]|2[0-4]\d|[01]?\d\d?)(?:\.(?:25[0-5]|2[0-4]\d|[01]?\d\d?)){3}\b",
                "<IP>",
            ),
            // Phone, the non-`+` shapes. These replace `\+?\d[\d\s().\-]{8,}\d`, which matched any
            // ISO date (2026-07-01), any dotted/dashed version (1.2.3-4.5.6) and ran across
            // whitespace into a following time. `.` is a separator only in the tight 3-3-4 grouping;
            // everywhere else it is what made version strings and quads collateral.
            r(
                "phone",
                r"\(\d{2,4}\)[ \-]?\d{2,4}(?:[ \-]?\d{2,4}){1,3}\b",
                "<PHONE>",
            ),
            r("phone", r"\b\d{3}[ \-.]\d{3}[ \-.]\d{4}\b", "<PHONE>"),
        ]
    })
}

/// Scrub structured PII from `text`, returning the cleaned text and a redaction count.
pub fn scrub(text: &str) -> ScrubResult {
    let mut out = text.to_string();
    let mut redactions = 0usize;
    for rule in rules() {
        let count = rule.re.find_iter(&out).count();
        if count > 0 {
            out = rule.re.replace_all(&out, rule.placeholder).into_owned();
            redactions += count;
        }
    }
    ScrubResult {
        text: out,
        redactions,
    }
}

mod stamp;

pub use stamp::{rules_fingerprint, scrub_detailed, ScrubReport};

#[cfg(test)]
mod export;
#[cfg(test)]
mod tests;
