//! The declared alias table — the last step of [`crate::model_id::canonicalize_with`].
//!
//! Split from `model_id` because it is a *data* concern (loading and looking up declarations) rather
//! than an algorithm, and because the price seed, the collective loader and the canonicalizer all
//! hold one.

use std::collections::HashMap;

/// Declared model aliases: `alias → canonical family`. Keys may be bare (`gemini-2.5-pro-002`) or
/// provider-qualified (`google/gemini-2.5-pro-002`); a qualified key wins when both exist.
///
/// Deliberately a *declaration*, never a heuristic: an identity absent from the table passes through
/// unchanged, so a model released tomorrow is never silently merged into an older one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AliasTable {
    entries: HashMap<String, String>,
}

impl AliasTable {
    /// Build from `(alias, canonical)` pairs; keys are canonicalized the same way a lookup is.
    pub fn from_pairs<I, A, B>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (A, B)>,
        A: AsRef<str>,
        B: AsRef<str>,
    {
        let entries = pairs
            .into_iter()
            .map(|(a, c)| {
                (
                    a.as_ref().trim().to_ascii_lowercase(),
                    c.as_ref().trim().to_ascii_lowercase(),
                )
            })
            .filter(|(a, c)| !a.is_empty() && !c.is_empty() && a != c)
            .collect();
        Self { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Every declared canonical target, for the "an alias must point at something we price" test.
    pub fn targets(&self) -> impl Iterator<Item = &str> {
        self.entries.values().map(String::as_str)
    }

    /// Resolve `model` (already canonical-cased) for `provider`, or `None` when undeclared.
    pub fn resolve(&self, provider: &str, model: &str) -> Option<&str> {
        self.entries
            .get(&format!("{provider}/{model}"))
            .or_else(|| self.entries.get(model))
            .map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_qualified_key_wins_over_a_bare_one() {
        let t = AliasTable::from_pairs([
            ("g-legacy", "gemini-2.5-flash"),
            ("google/g-legacy", "gemini-2.5-pro"),
        ]);
        assert_eq!(t.resolve("google", "g-legacy"), Some("gemini-2.5-pro"));
        assert_eq!(t.resolve("acme", "g-legacy"), Some("gemini-2.5-flash"));
        assert_eq!(t.resolve("google", "unheard-of"), None);
    }

    #[test]
    fn empty_and_self_referential_entries_are_dropped() {
        // A self-alias is not a declaration, it is a no-op that would make `targets()` lie about
        // what the table actually collapses.
        let t = AliasTable::from_pairs([("gpt-4o", "gpt-4o"), ("  ", "x"), ("y", "")]);
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn keys_and_targets_are_case_folded() {
        let t = AliasTable::from_pairs([("GPT-4O-Legacy", "GPT-4o-Mini")]);
        assert_eq!(t.resolve("openai", "gpt-4o-legacy"), Some("gpt-4o-mini"));
        assert_eq!(t.targets().collect::<Vec<_>>(), vec!["gpt-4o-mini"]);
    }
}
