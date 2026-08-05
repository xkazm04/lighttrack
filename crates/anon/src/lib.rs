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

struct Rule {
    re: Regex,
    placeholder: &'static str,
}

fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let r = |p: &str, ph: &'static str| Rule {
            re: Regex::new(p).expect("valid regex"),
            placeholder: ph,
        };
        vec![
            r(
                r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}",
                "<EMAIL>",
            ),
            r(r"\b[A-Z]{2}\d{2}[A-Z0-9]{10,30}\b", "<IBAN>"),
            r(r"\b\d{3}-\d{2}-\d{4}\b", "<SSN>"),
            r(r"\bsk-[A-Za-z0-9_\-]{16,}\b", "<SECRET>"),
            r(r"\bAKIA[0-9A-Z]{12,}\b", "<SECRET>"),
            r(r"\b[0-9a-fA-F]{32,}\b", "<SECRET>"),
            // A leading `+` is a phone signal no card number carries, so the two E.164 shapes are
            // tried before the card rule — otherwise a 13+ digit international number types as <CC>.
            r(
                r"\+\d{1,3}(?:[ \-](?:\(\d{1,4}\)|\d{2,4})){2,5}\b",
                "<PHONE>",
            ),
            r(r"\+\d{1,3}[ \-]?\d{7,14}\b", "<PHONE>"),
            // Anchored on a digit at *both* ends: the old `(?:\d[ \-]?){13,19}` let the last
            // repetition swallow its trailing separator, so `card 4111 1111 1111 1111 was` came back
            // as `card <CC>was`.
            r(r"\b\d(?:[ \-]?\d){12,18}\b", "<CC>"),
            // Real octets only. `\d{1,3}` also claimed impossible quads (999.999.999.999), which is
            // pure corruption — nothing rejected here was ever an address.
            r(
                r"\b(?:25[0-5]|2[0-4]\d|[01]?\d\d?)(?:\.(?:25[0-5]|2[0-4]\d|[01]?\d\d?)){3}\b",
                "<IP>",
            ),
            // Phone, the non-`+` shapes. These replace `\+?\d[\d\s().\-]{8,}\d`, which matched any
            // ISO date (2026-07-01), any dotted/dashed version (1.2.3-4.5.6) and ran across
            // whitespace into a following time. `.` is a separator only in the tight 3-3-4 grouping;
            // everywhere else it is what made version strings and quads collateral.
            r(
                r"\(\d{2,4}\)[ \-]?\d{2,4}(?:[ \-]?\d{2,4}){1,3}\b",
                "<PHONE>",
            ),
            r(r"\b\d{3}[ \-.]\d{3}[ \-.]\d{4}\b", "<PHONE>"),
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

#[cfg(test)]
mod tests;
