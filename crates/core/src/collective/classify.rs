//! Benchmark-name → coarse task-type classification. Publishing only the fixed vocabulary (never a
//! raw benchmark name) is what keeps the digest from leaking project-specific naming.

/// Keyword stems per bucket, most specific first; the first bucket with a hit wins. A stem matches
/// when a **word** of the lowercased text starts with it — not when it appears anywhere inside one.
/// Substring matching was the defect this replaces: `ner` (named-entity) sat inside *generation*,
/// so every "generation" benchmark published as `extraction` and the `generation` bucket was
/// unreachable; `rag` sat inside *storage*, `qa` inside *qatar*, `plan` inside *explanation*.
/// Every label here is a member of [`TASK_TYPES`] and every non-default member is reachable —
/// both pinned by test, since a typo would publish a bucket outside the vocabulary.
const STEMS: &[(&str, &[&str])] = &[
    ("summarization", &["summ", "tldr", "abstract"]),
    ("translation", &["translat", "localiz", "i18n"]),
    ("extraction", &["extract", "pars", "ner", "entit"]),
    (
        "classification",
        &["classif", "categor", "intent", "sentiment", "moderat"],
    ),
    (
        "coding",
        &[
            "code", "coding", "program", "sql", "bug", "debug", "refactor",
        ],
    ),
    ("rag", &["rag", "retriev", "grounded", "citation"]),
    ("reasoning", &["reason", "math", "logic", "plan", "agent"]),
    ("qa", &["qa", "question", "answer", "faq", "support"]),
    (
        "generation",
        &["generat", "writ", "draft", "compos", "creative"],
    ),
];

/// Classify a benchmark `name` (with an optional explicit `hint`, e.g. a tag) into the fixed
/// [`TASK_TYPES`] vocabulary. Word-prefix match on the lowercased text; defaults to `general`.
/// Always returns one of [`TASK_TYPES`], so the published bucket never carries custom naming.
pub fn task_type_from(name: &str, hint: Option<&str>) -> String {
    let hay = format!("{} {}", name, hint.unwrap_or("")).to_lowercase();
    let words: Vec<&str> = hay
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    for (label, stems) in STEMS {
        if words.iter().any(|w| stems.iter().any(|s| w.starts_with(s))) {
            return (*label).to_string();
        }
    }
    "general".to_string()
}

#[cfg(test)]
mod tests {
    use super::super::TASK_TYPES;
    use super::*;

    #[test]
    fn classifier_returns_fixed_vocabulary() {
        assert_eq!(
            task_type_from("Nightly summarization eval", None),
            "summarization"
        );
        assert_eq!(task_type_from("SQL bug-fix bench", None), "coding");
        assert_eq!(task_type_from("Customer FAQ answering", None), "qa");
        assert_eq!(task_type_from("Grounded RAG citations", None), "rag");
        assert_eq!(
            task_type_from("widget-prod-xyz", Some("i18n")),
            "translation"
        );
        // Unknown → general, and always a member of the vocabulary.
        let t = task_type_from("widget-prod-xyz", None);
        assert_eq!(t, "general");
        assert!(TASK_TYPES.contains(&t.as_str()));
    }

    /// The misroutes substring matching produced, each fixed by matching on word prefixes.
    #[test]
    fn a_stem_inside_another_word_does_not_claim_the_benchmark() {
        assert_eq!(task_type_from("Email generation", None), "generation");
        assert_eq!(task_type_from("Story generator", None), "generation");
        assert_eq!(task_type_from("Storage tiering", None), "general");
        assert_eq!(task_type_from("Owner FAQ", None), "qa");
        assert_eq!(task_type_from("Explanation quality", None), "general");
        assert_eq!(task_type_from("Debug helper", None), "coding");
    }

    /// The stem table and the published vocabulary are two lists that must agree: a label outside
    /// `TASK_TYPES` would publish a bucket the hub never expects, and an unreachable member is a
    /// bucket nothing can ever land in.
    #[test]
    fn every_stem_label_is_in_the_vocabulary_and_every_bucket_is_reachable() {
        for (label, _) in STEMS {
            assert!(
                TASK_TYPES.contains(label),
                "{label} is not a published task type"
            );
        }
        for t in TASK_TYPES.iter().filter(|t| **t != "general") {
            assert!(
                STEMS.iter().any(|(label, _)| label == t),
                "{t} has no stem, so no benchmark can ever be classified into it"
            );
        }
    }
}
