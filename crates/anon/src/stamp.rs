//! What a scrub did, and which rules did it.
//!
//! The scrubber used to return a bare count that every caller logged and dropped, so a database
//! became an indistinguishable mix of raw and scrubbed rows — and D14 already names the one class of
//! defect this product cannot observe: a scrub that rewrote the evidence a judge later read. A count
//! is not enough on its own either, because the rule list is not fixed: it has already changed shape
//! once (the D14 phone rules), and "3 spans redacted" means a different thing before and after.
//!
//! So two things travel with a scrubbed row: [`ScrubReport`] (how many spans, and of what kind) and
//! [`rules_fingerprint`] (which rule set produced them). Together they make "was this row scrubbed,
//! by what, and how hard" a query instead of a guess.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use crate::rules;

/// Redactions from one scrub, broken down by the placeholder they were replaced with.
///
/// The placeholder, not the rule `kind`, is the grouping key: it is what a reader of the stored text
/// actually sees, and several rules deliberately share one (a secret has three shapes). A caller that
/// only wants the total reads [`ScrubReport::redactions`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubReport {
    /// Total spans replaced across every rule.
    pub redactions: usize,
    /// `<EMAIL>` → 2, `<CC>` → 1, … Only placeholders that actually fired appear; a zero entry
    /// would read as "we looked and there were none", which is the same statement as absence here.
    pub by_placeholder: BTreeMap<&'static str, usize>,
}

/// Scrub `text`, returning the cleaned text and the per-placeholder breakdown.
///
/// [`crate::scrub`] is this function with the breakdown discarded; the two run the same rules in the
/// same order, so a count from one is the count from the other.
pub fn scrub_detailed(text: &str) -> (String, ScrubReport) {
    let mut out = text.to_string();
    let mut report = ScrubReport::default();
    for rule in rules() {
        let count = rule.re.find_iter(&out).count();
        if count > 0 {
            out = rule.re.replace_all(&out, rule.placeholder).into_owned();
            report.redactions += count;
            *report.by_placeholder.entry(rule.placeholder).or_default() += count;
        }
    }
    (out, report)
}

/// A stable short digest of the rule set **in evaluation order** — the first 12 hex characters of the
/// sha256 over each rule's pattern and placeholder.
///
/// Short on purpose: this is stamped into `metadata` on every scrubbed event, so a full 64-character
/// digest would be 64 bytes per row to distinguish a handful of rule-set generations. 12 hex
/// characters is 48 bits — collision-free for the number of rule sets this project will ever have,
/// and short enough to read in a log line. Order is included because order is part of the contract
/// (an IP must be matched before the phone rules see it), so a reordering that changes what gets
/// redacted changes the fingerprint too.
pub fn rules_fingerprint() -> &'static str {
    static FP: OnceLock<String> = OnceLock::new();
    FP.get_or_init(|| {
        let mut h = Sha256::new();
        for rule in rules() {
            h.update(rule.re.as_str().as_bytes());
            h.update([0u8]);
            h.update(rule.placeholder.as_bytes());
            h.update([0u8]);
        }
        let digest = h.finalize();
        digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fingerprint_is_stable_short_hex() {
        let fp = rules_fingerprint();
        assert_eq!(fp.len(), 12, "12 hex characters: {fp}");
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(fp, rules_fingerprint(), "computed once, stable");
    }

    /// The fingerprint is a function of the rules, not of the machine: two different rule sets must
    /// not share one, and reordering must move it (order decides what gets redacted).
    #[test]
    fn a_different_rule_set_would_hash_differently() {
        fn fp_of(pairs: &[(&str, &str)]) -> String {
            let mut h = Sha256::new();
            for (p, ph) in pairs {
                h.update(p.as_bytes());
                h.update([0u8]);
                h.update(ph.as_bytes());
                h.update([0u8]);
            }
            h.finalize()
                .iter()
                .take(6)
                .map(|b| format!("{b:02x}"))
                .collect()
        }
        let a = fp_of(&[("x", "<X>"), ("y", "<Y>")]);
        let b = fp_of(&[("y", "<Y>"), ("x", "<X>")]);
        let c = fp_of(&[("x", "<X>")]);
        assert_ne!(a, b, "order is part of the contract");
        assert_ne!(a, c);
    }

    #[test]
    fn the_breakdown_groups_by_placeholder_and_sums_to_the_total() {
        let (text, report) = scrub_detailed("a@b.com and c@d.com called +1 555 123 4567");
        assert!(!text.contains("a@b.com"));
        assert_eq!(report.by_placeholder.get("<EMAIL>"), Some(&2));
        assert_eq!(report.by_placeholder.get("<PHONE>"), Some(&1));
        assert_eq!(
            report.redactions,
            report.by_placeholder.values().sum::<usize>()
        );
    }

    /// Clean text produces an empty report — not a map of zeros.
    #[test]
    fn nothing_to_redact_is_an_empty_breakdown() {
        let (text, report) = scrub_detailed("just a sentence");
        assert_eq!(text, "just a sentence");
        assert_eq!(report.redactions, 0);
        assert!(report.by_placeholder.is_empty());
    }

    /// `scrub` and `scrub_detailed` must never disagree — the stamp on a row is the count the
    /// ingest path acted on.
    #[test]
    fn the_detailed_scrub_matches_the_plain_one() {
        let input = "mail a@b.com card 4111 1111 1111 1111 ip 10.0.0.1";
        let plain = crate::scrub(input);
        let (text, report) = scrub_detailed(input);
        assert_eq!(plain.text, text);
        assert_eq!(plain.redactions, report.redactions);
    }
}
