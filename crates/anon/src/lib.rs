//! Heuristic PII scrubbing for dataset building.
//!
//! This is the **regex pass** of the hybrid anonymization pipeline (see
//! `docs/BENCHMARK_FRAMEWORK.md` §1): structured PII with reliable shapes — emails, IBANs, national
//! IDs, secrets, card numbers, IPs, phone numbers — replaced with typed placeholders. Free-text PII
//! (names, orgs, locations) is left to the optional LLM pass in the runner.
//!
//! Rules run in a fixed order (most specific → least) so e.g. an IP isn't eaten by the phone rule.

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
            r(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}", "<EMAIL>"),
            r(r"\b[A-Z]{2}\d{2}[A-Z0-9]{10,30}\b", "<IBAN>"),
            r(r"\b\d{3}-\d{2}-\d{4}\b", "<SSN>"),
            r(r"\bsk-[A-Za-z0-9_\-]{16,}\b", "<SECRET>"),
            r(r"\bAKIA[0-9A-Z]{12,}\b", "<SECRET>"),
            r(r"\b[0-9a-fA-F]{32,}\b", "<SECRET>"),
            r(r"\b(?:\d[ \-]?){13,19}\b", "<CC>"),
            r(r"\b\d{1,3}(?:\.\d{1,3}){3}\b", "<IP>"),
            r(r"\+?\d[\d\s().\-]{8,}\d", "<PHONE>"),
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
mod tests {
    use super::*;

    #[test]
    fn scrubs_common_pii() {
        let s = scrub(
            "Contact john.doe@example.com or call +1 (415) 555-2671. \
             Card 4111 1111 1111 1111, server 10.0.0.1, key sk-abcd1234efgh5678ijkl.",
        );
        assert!(s.text.contains("<EMAIL>"), "{}", s.text);
        assert!(s.text.contains("<PHONE>"), "{}", s.text);
        assert!(s.text.contains("<CC>"), "{}", s.text);
        assert!(s.text.contains("<IP>"), "{}", s.text);
        assert!(s.text.contains("<SECRET>"), "{}", s.text);
        assert!(!s.text.contains("john.doe@example.com"));
        assert!(!s.text.contains("4111"));
        assert!(s.redactions >= 5, "redactions={}", s.redactions);
    }

    #[test]
    fn leaves_clean_text_untouched() {
        let s = scrub("The capital of France is Paris.");
        assert_eq!(s.text, "The capital of France is Paris.");
        assert_eq!(s.redactions, 0);
    }
}
