//! Model-identity normalization for the collective network.
//!
//! Model identity arrives as free text, so `gpt-4o`, `openai/gpt-4o`, and `gpt-4o-2024-08-06` would
//! otherwise be three distinct leaderboard rows. This canonicalizes a `(provider, model)` pair at
//! ingest: it strips a redundant `provider/` prefix from the model, then applies an alias table
//! (seeded from `config/model_aliases.json`) that collapses provider synonyms and dated model variants
//! to their family — but **only where the table says so**. An identity absent from the table passes
//! through unchanged (minus any stripped prefix), so a new model is never silently mis-merged.

use std::collections::HashMap;

use serde::Deserialize;

/// Canonicalization table. Keys are matched case-insensitively; values are the canonical spellings.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelAliases {
    /// Provider synonym → canonical provider (e.g. `azure-openai` → `openai`).
    #[serde(default)]
    providers: HashMap<String, String>,
    /// Exact model (after any `provider/` prefix is stripped) → canonical family (e.g.
    /// `gpt-4o-2024-08-06` → `gpt-4o`).
    #[serde(default)]
    models: HashMap<String, String>,
}

impl ModelAliases {
    /// Parse an alias table from JSON. Unknown top-level keys (e.g. `_meta`) are ignored.
    pub fn from_json_str(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Canonicalize a `(provider, model)` pair. Pure and total: an unmapped identity returns unchanged
    /// except that a redundant `provider/` prefix on the model is stripped.
    pub fn normalize(&self, provider: &str, model: &str) -> (String, String) {
        let provider = provider.trim();
        let mut model = model.trim();
        // Strip a leading `something/` prefix from the model (e.g. `openai/gpt-4o` → `gpt-4o`), but only
        // when both halves are non-empty so we never turn a real name into "".
        if let Some((pre, rest)) = model.split_once('/') {
            if !pre.trim().is_empty() && !rest.trim().is_empty() {
                model = rest.trim();
            }
        }
        let canon_provider = self
            .providers
            .get(&provider.to_lowercase())
            .cloned()
            .unwrap_or_else(|| provider.to_string());
        let canon_model = self
            .models
            .get(&model.to_lowercase())
            .cloned()
            .unwrap_or_else(|| model.to_string());
        (canon_provider, canon_model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> ModelAliases {
        ModelAliases::from_json_str(
            r#"{
                "_meta": {"note": "ignored"},
                "providers": {"azure-openai": "openai", "google-vertex": "google"},
                "models": {
                    "gpt-4o-2024-08-06": "gpt-4o",
                    "gpt-4o": "gpt-4o",
                    "claude-3-5-sonnet-20241022": "claude-3-5-sonnet"
                }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn strips_provider_prefix_and_collapses_dated_variants() {
        let a = table();
        // provider/ prefix stripped, then family collapse.
        assert_eq!(a.normalize("openai", "openai/gpt-4o"), ("openai".into(), "gpt-4o".into()));
        assert_eq!(
            a.normalize("openai", "gpt-4o-2024-08-06"),
            ("openai".into(), "gpt-4o".into())
        );
        // provider synonym mapped.
        assert_eq!(a.normalize("azure-openai", "gpt-4o"), ("openai".into(), "gpt-4o".into()));
        assert_eq!(
            a.normalize("google-vertex", "gemini-1.5-pro"),
            ("google".into(), "gemini-1.5-pro".into())
        );
    }

    #[test]
    fn unknown_identities_pass_through_unchanged() {
        let a = table();
        // Not in the table → unchanged (only the prefix stripping applies).
        assert_eq!(
            a.normalize("anthropic", "some-new-model-v9"),
            ("anthropic".into(), "some-new-model-v9".into())
        );
        // Empty table normalizes nothing but still strips the prefix.
        let empty = ModelAliases::default();
        assert_eq!(empty.normalize("openai", "openai/gpt-4o"), ("openai".into(), "gpt-4o".into()));
        assert_eq!(empty.normalize("x", "y-2024-01-01"), ("x".into(), "y-2024-01-01".into()));
    }

    #[test]
    fn case_insensitive_keys() {
        let a = table();
        assert_eq!(a.normalize("Azure-OpenAI", "GPT-4O-2024-08-06"), ("openai".into(), "gpt-4o".into()));
    }
}
