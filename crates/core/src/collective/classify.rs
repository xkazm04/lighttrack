//! Benchmark-name → coarse task-type classification. Publishing only the fixed vocabulary (never a
//! raw benchmark name) is what keeps the digest from leaking project-specific naming.

/// Classify a benchmark `name` (with an optional explicit `hint`, e.g. a tag) into the fixed
/// [`TASK_TYPES`] vocabulary. Keyword match on the lowercased text; defaults to `general`. Always
/// returns one of [`TASK_TYPES`], so the published bucket never carries custom naming.
pub fn task_type_from(name: &str, hint: Option<&str>) -> String {
    let hay = format!("{} {}", name, hint.unwrap_or("")).to_lowercase();
    // Most specific → least; first hit wins.
    let table: &[(&str, &[&str])] = &[
        ("summarization", &["summ", "tldr", "abstract"]),
        ("translation", &["translat", "localiz", "i18n"]),
        ("extraction", &["extract", "parse", "ner", "entit"]),
        ("classification", &["classif", "categor", "intent", "sentiment", "moderation"]),
        ("coding", &["code", "coding", "program", "sql", "bug", "refactor"]),
        ("rag", &["rag", "retriev", "grounded", "citation"]),
        ("reasoning", &["reason", "math", "logic", "plan", "agent"]),
        ("qa", &["qa", "question", "answer", "faq", "support"]),
        ("generation", &["generat", "writ", "draft", "compose", "creative"]),
    ];
    for (label, keys) in table {
        if keys.iter().any(|k| hay.contains(k)) {
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
        assert_eq!(task_type_from("Nightly summarization eval", None), "summarization");
        assert_eq!(task_type_from("SQL bug-fix bench", None), "coding");
        assert_eq!(task_type_from("Customer FAQ answering", None), "qa");
        assert_eq!(task_type_from("Grounded RAG citations", None), "rag");
        // Unknown → general, and always a member of the vocabulary.
        let t = task_type_from("widget-prod-xyz", None);
        assert_eq!(t, "general");
        assert!(TASK_TYPES.contains(&t.as_str()));
    }
}
