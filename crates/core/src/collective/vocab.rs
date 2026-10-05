//! Every categorical value a contributor may put on the wire, and the clamp that owns it.
//!
//! **Why this module exists.** Classify at the source, clamp at the hub: a contributor's classifier
//! is total over a closed vocabulary ([`task_type_from`](super::task_type_from)), but a hub cannot
//! trust that it was — the hub's clamp is what makes the closure real rather than aspirational. That
//! rule used to be implemented three different places and skipped in a fourth: `canon_determinism`
//! lived in [`rigor`](super::rigor), the judge-tag clamp lived privately inside the API's ingest
//! handler, and `task_type` — the field the rule is actually named for — had no clamp at all, so a
//! free-text benchmark name reached the stored table and the public `?task_type=` filter.
//!
//! Three homes for one rule means the *fourth* wire enum gets added without one too, because nothing
//! in the build notices. So this module owns all of them, and [`WIRE_CATEGORICALS`] below is checked
//! against the serialized schema of [`ModelDigestEntry`](super::ModelDigestEntry) by a test: adding a
//! new string- or enum-valued wire field fails the build until somebody names the clamp that governs
//! it.
//!
//! **Why clamping and not rejecting.** An unrecognized value becomes the vocabulary's own "not
//! recorded" member — `None` for determinism, `general` for a task type — never a new member. A
//! contributor must not be able to widen a vocabulary by sending a creative string: a free-text label
//! is a confession of product line and internal roadmap, and it is also a per-contributor
//! fingerprint sharp enough to defeat the k-anonymity floors that sit on top of it.

use super::TASK_TYPES;

/// Clamp a contributed determinism stamp to the closed rigor vocabulary. Re-exported here so there
/// is one place to look for "what may a contributor say", with the fold rules beside the levels.
pub use super::rigor::canon_determinism;

/// The fallback member of [`TASK_TYPES`]: what an unrecognized label becomes.
///
/// A task type is part of the hub's storage key, so unlike determinism it cannot degrade to `None`;
/// the vocabulary carries its own catch-all bucket for exactly this reason, and the classifier on the
/// contributor side already defaults to it.
pub const TASK_TYPE_FALLBACK: &str = "general";

/// Clamp a contributed task-type label to [`TASK_TYPES`].
///
/// Case and surrounding whitespace are **folded**, not punished: `" QA "` is the member `qa`, because
/// a spelling difference is not a new bucket and collapsing it to `general` would throw away evidence
/// the contributor really did classify. Anything that is not a member after folding becomes
/// [`TASK_TYPE_FALLBACK`].
pub fn canon_task_type(s: &str) -> String {
    let s = s.trim().to_lowercase();
    TASK_TYPES
        .iter()
        .find(|t| **t == s)
        .map_or_else(|| TASK_TYPE_FALLBACK.to_string(), |t| (*t).to_string())
}

/// `true` when `canon_task_type` had to fall back — i.e. the contributed label was outside the
/// vocabulary rather than merely spelled differently. Separated from the clamp itself so the ingest
/// ack can **disclose** how many labels the hub refused to take at face value.
pub fn task_type_is_clamped(s: &str) -> bool {
    let s = s.trim().to_lowercase();
    !TASK_TYPES.iter().any(|t| **t == s)
}

/// Clamp a contributed judge tag to the known judge vocabulary, so a poster can't inject arbitrary
/// judge labels. The vocabulary is [`ProviderFamily`](crate::ProviderFamily) plus `mixed` — the one
/// value a *merge* produces and no single judge ever is, which is why it cannot come from
/// `judge_family`.
pub fn canon_judge_provider(j: &str) -> String {
    if j.trim().eq_ignore_ascii_case("mixed") {
        return "mixed".to_string();
    }
    crate::judge_family(j).as_str().to_string()
}

/// Which rule owns a wire field's value space. Not decoration: it is the vocabulary of
/// [`WIRE_CATEGORICALS`], and the point of naming it per field is that "nothing" is not an option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clamp {
    /// Clamped here to a closed vocabulary: an unrecognized value becomes the vocabulary's "not
    /// recorded" member, never a new one.
    Vocabulary,
    /// Not a vocabulary — a model identity, folded to a canonical spelling through the hub's declared
    /// alias table (`ModelAliases::normalize`) so equivalent spellings merge into one row.
    Normalized,
    /// Not a vocabulary — an opaque one-way hash, bounded in length so it cannot carry a payload.
    Bounded,
}

/// Every categorical (string- or enum-valued) field [`ModelDigestEntry`](super::ModelDigestEntry)
/// carries, paired with the clamp that owns it.
///
/// The left column is **derived** from the serialized schema by the test below rather than trusted,
/// so this table cannot quietly fall behind the struct: a new `String`/enum wire field without an
/// entry here is a red build, which is the only reliable way to notice an unclamped channel.
pub const WIRE_CATEGORICALS: &[(&str, Clamp)] = &[
    ("provider", Clamp::Normalized),
    ("model", Clamp::Normalized),
    ("task_type", Clamp::Vocabulary),
    ("judge_provider", Clamp::Vocabulary),
    ("rubric_fingerprint", Clamp::Bounded),
    ("determinism", Clamp::Vocabulary),
    ("frozen_dataset", Clamp::Vocabulary),
    ("significance_tested", Clamp::Vocabulary),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_off_vocabulary_task_type_becomes_the_fallback_and_says_so() {
        assert_eq!(canon_task_type("claims-triage-v3"), "general");
        assert!(task_type_is_clamped("claims-triage-v3"));
        // A 4KB label, a unicode one and an empty one are all just "not a member".
        assert_eq!(canon_task_type(&"x".repeat(4096)), "general");
        assert_eq!(canon_task_type("\u{65e5}\u{672c}\u{8a9e}"), "general");
        assert_eq!(canon_task_type(""), "general");
        // Vocabulary-adjacent is still outside it.
        assert_eq!(canon_task_type("summarizations"), "general");
        assert_eq!(canon_task_type("general-v2"), "general");
    }

    #[test]
    fn case_and_whitespace_are_folded_not_clamped() {
        assert_eq!(canon_task_type(" QA "), "qa");
        assert_eq!(canon_task_type("Summarization"), "summarization");
        assert!(!task_type_is_clamped(" QA "));
        // The fallback itself is a member, so asking for it is not a clamp.
        assert!(!task_type_is_clamped("general"));
        assert_eq!(canon_task_type("general"), "general");
    }

    #[test]
    fn every_clamped_task_type_is_a_vocabulary_member() {
        for s in [
            "",
            "   ",
            "QA",
            "mixed",
            "unknown",
            "../../etc/passwd",
            "\u{1f642}",
            "reasoning",
        ] {
            let t = canon_task_type(s);
            assert!(
                TASK_TYPES.contains(&t.as_str()),
                "{s:?} clamped to {t:?}, which is outside the vocabulary"
            );
        }
    }

    #[test]
    fn an_unknown_judge_label_cannot_widen_the_judge_vocabulary() {
        assert_eq!(canon_judge_provider("weird-label"), "unknown");
        assert_eq!(canon_judge_provider("Mixed"), "mixed");
        assert_eq!(canon_judge_provider("anthropic/claude-haiku"), "anthropic");
    }

    /// The derived list. `ModelDigestEntry` is what a contributor controls; every field of it whose
    /// value space is a string or an enum is a channel, and a channel with no named clamp is how the
    /// next free-text leak ships. Resolving one level of `$ref` is enough — `Coverage` is the only
    /// named enum on the entry.
    #[test]
    fn every_categorical_wire_field_names_a_clamp() {
        let schema =
            serde_json::to_value(schemars::schema_for!(super::super::ModelDigestEntry)).unwrap();
        let defs = schema
            .get("$defs")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let props = schema["properties"].as_object().expect("object schema");

        let is_stringy = |s: &serde_json::Value| -> bool {
            let names_string = |t: &serde_json::Value| match t {
                serde_json::Value::String(s) => s == "string",
                serde_json::Value::Array(a) => a.iter().any(|v| v == "string"),
                _ => false,
            };
            if s.get("type").is_some_and(names_string) {
                return true;
            }
            // A `oneOf` of string `const`s is how schemars renders a unit-variant enum.
            s.get("oneOf").and_then(|o| o.as_array()).is_some_and(|a| {
                a.iter()
                    .any(|v| v.get("const").is_some() && v.get("type").is_some_and(names_string))
            })
        };

        let mut found: Vec<&str> = props
            .iter()
            .filter(|(_, s)| {
                let resolved = s
                    .get("$ref")
                    .and_then(|r| r.as_str())
                    .and_then(|r| r.rsplit('/').next())
                    .and_then(|name| defs.get(name))
                    .cloned();
                is_stringy(resolved.as_ref().unwrap_or(s))
            })
            .map(|(k, _)| k.as_str())
            .collect();
        found.sort_unstable();

        let mut declared: Vec<&str> = WIRE_CATEGORICALS.iter().map(|(f, _)| *f).collect();
        declared.sort_unstable();

        assert_eq!(
            found, declared,
            "a categorical wire field with no declared clamp is an unclamped channel: add it to \
             WIRE_CATEGORICALS and give it a clamp (or remove the stale entry)"
        );
    }
}
