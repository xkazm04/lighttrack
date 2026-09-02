//! Model-identity normalization for the collective network — now a **shim** over the one
//! canonicalizer.
//!
//! This module used to own its own rules (a provider synonym map, an exact-model map, prefix
//! stripping). Those are [`crate::model_id::canonicalize`] now, and the declared collapses live in
//! the price seed's per-model `aliases` lists, so an alias can no longer name a model nothing
//! prices. What remains here is the loader for that table plus the `(provider, model)` shape the
//! collective ingest path wants.
//!
//! The conservative rule is unchanged and load-bearing: an identity absent from the declared table
//! passes through unchanged (minus the derivable normalization), and rows never merge on *family* —
//! a leaderboard that merged `openrouter` into `anthropic` would publish a number nobody measured.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

use crate::model_id::{canonicalize_with, AliasTable};

/// The declared identity table, loaded from `config/pricing.json` (or any file of that shape).
#[derive(Debug, Clone, Default)]
pub struct ModelAliases {
    table: AliasTable,
}

/// `{ "models": { "<key>": <value> } }` — the one shape both the price seed and the legacy
/// `model_aliases.json` share; the *values* differ, so they are read as `Value`.
#[derive(Debug, Default, Deserialize)]
struct AliasFile {
    #[serde(default)]
    models: HashMap<String, Value>,
}

impl ModelAliases {
    /// Parse an alias table from JSON. Two shapes are accepted, because the same env var
    /// (`LIGHTTRACK_MODEL_ALIASES`) may still point at a pre-M8 file:
    /// - the price seed: `{"models": {"openai/gpt-4o-mini": {"aliases": ["…"], …}}}`;
    /// - the legacy table: `{"models": {"gpt-4o-2024-08-06": "gpt-4o"}}` (its `providers` map is
    ///   ignored — provider synonyms are declared once, in `model_id`).
    ///
    /// Unknown top-level keys (`_meta`) are ignored.
    pub fn from_json_str(s: &str) -> Result<Self, serde_json::Error> {
        let file: AliasFile = serde_json::from_str(s)?;
        let mut pairs: Vec<(String, String)> = Vec::new();
        for (key, value) in &file.models {
            let canonical = key.split_once('/').map_or(key.as_str(), |(_, m)| m);
            match value {
                // Legacy shape: the key *is* the alias and the value is its target.
                Value::String(target) => pairs.push((key.clone(), target.clone())),
                // Price-seed shape: the key is the canonical model, `aliases` are its other spellings.
                Value::Object(o) => {
                    if let Some(list) = o.get("aliases").and_then(Value::as_array) {
                        for a in list.iter().filter_map(Value::as_str) {
                            pairs.push((a.to_string(), canonical.to_string()));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(Self {
            table: AliasTable::from_pairs(pairs),
        })
    }

    /// Build from an already-loaded table (e.g. a `PriceBook`'s).
    pub fn from_table(table: AliasTable) -> Self {
        Self { table }
    }

    pub fn table(&self) -> &AliasTable {
        &self.table
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    /// Canonicalize a `(provider, model)` pair for the leaderboard. Pure and total.
    pub fn normalize(&self, provider: &str, model: &str) -> (String, String) {
        let id = canonicalize_with(provider, model, &self.table);
        (id.provider.as_str().to_string(), id.family)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> ModelAliases {
        ModelAliases::from_json_str(
            r#"{
                "_meta": {"note": "ignored"},
                "models": {
                    "google/gemini-2.5-pro": {
                        "input_per_mtok": 1.25,
                        "output_per_mtok": 10.0,
                        "aliases": ["gemini-2.5-pro-002"]
                    },
                    "openai/gpt-4o-mini": { "input_per_mtok": 0.15, "output_per_mtok": 0.6 }
                }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn strips_provider_prefix_and_collapses_dated_variants() {
        let a = table();
        assert_eq!(
            a.normalize("openai", "openai/gpt-4o"),
            ("openai".into(), "gpt-4o".into())
        );
        // Dated variants are derivable now — no table entry needed for either date spelling.
        assert_eq!(
            a.normalize("openai", "gpt-4o-2024-08-06"),
            ("openai".into(), "gpt-4o".into())
        );
        assert_eq!(
            a.normalize("anthropic", "claude-3-5-sonnet-20241022"),
            ("anthropic".into(), "claude-3-5-sonnet".into())
        );
        // Declared aliases still apply, and provider synonyms fold.
        assert_eq!(
            a.normalize("google-vertex", "gemini-2.5-pro-002"),
            ("google".into(), "gemini-2.5-pro".into())
        );
        assert_eq!(
            a.normalize("azure-openai", "gpt-4o"),
            ("openai".into(), "gpt-4o".into())
        );
    }

    #[test]
    fn the_legacy_file_shape_still_loads() {
        let a = ModelAliases::from_json_str(
            r#"{"providers": {"azure": "openai"}, "models": {"g-old": "gemini-2.5-pro"}}"#,
        )
        .unwrap();
        assert_eq!(
            a.normalize("google", "g-old"),
            ("google".into(), "gemini-2.5-pro".into())
        );
    }

    #[test]
    fn unknown_identities_pass_through_unchanged() {
        let a = table();
        assert_eq!(
            a.normalize("anthropic", "some-new-model-v9"),
            ("anthropic".into(), "some-new-model-v9".into())
        );
        // An unmodeled provider keeps its own id — it is never merged into a family.
        assert_eq!(
            a.normalize("openrouter", "anthropic/claude-sonnet-5"),
            ("openrouter".into(), "claude-sonnet-5".into())
        );
        let empty = ModelAliases::default();
        assert_eq!(
            empty.normalize("openai", "openai/gpt-4o"),
            ("openai".into(), "gpt-4o".into())
        );
        assert_eq!(
            empty.normalize("x", "y-2024-01-01"),
            ("x".into(), "y".into())
        );
    }

    #[test]
    fn case_insensitive_keys() {
        assert_eq!(
            table().normalize("Azure-OpenAI", "GPT-4O-2024-08-06"),
            ("openai".into(), "gpt-4o".into())
        );
    }
}
