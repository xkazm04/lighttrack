use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{LtError, Result};
use crate::event::TokenUsage;
use crate::model_id::{canonicalize_with, AliasTable, ModelId};

/// A persisted price-book row (the DB-backed source of truth; `pricing.json` is just the seed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPriceRow {
    pub provider: String,
    pub model: String,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_mtok: Option<f64>,
    #[serde(default = "Utc::now")]
    pub effective_date: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
}

/// Per-model price, in USD per 1,000,000 tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPrice {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    /// Discounted rate for cached/prompt-cache input tokens. Falls back to `input_per_mtok` if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_mtok: Option<f64>,
    /// Other spellings that mean *this* model (`gemini-2.5-pro-002`).
    ///
    /// Seed-only, and deliberately declared here rather than in a second file: an alias table that
    /// lives beside the prices cannot name a model nothing prices, which is exactly what the old
    /// `model_aliases.json` did (7 of its 8 targets were unpriced).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

/// Which pricing lane a call uses. `Batch`/`Flex` select an alternate price-row variant when one
/// exists (`<model>@batch` / `<model>@flex`); otherwise they fall back to standard rates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PricingMode {
    #[default]
    Standard,
    Batch,
    Flex,
}

impl PricingMode {
    /// Parse a free-form mode hint: `batch` / `flex` (or `priority`) / anything-else → standard.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "batch" => PricingMode::Batch,
            "flex" | "priority" => PricingMode::Flex,
            _ => PricingMode::Standard,
        }
    }

    /// The price-row model-name suffix for this lane, if any.
    fn suffix(self) -> Option<&'static str> {
        match self {
            PricingMode::Standard => None,
            PricingMode::Batch => Some("@batch"),
            PricingMode::Flex => Some("@flex"),
        }
    }
}

/// A book of model prices keyed by `"<provider>/<model>"`.
///
/// Beyond plain `<model>` rows, a model may also have **variant** rows that encode a modifier in the
/// `model` name (stored like any other row — no schema change):
/// - `<model>@in>N`  — prompt-length tier: applies when input tokens exceed `N` (e.g.
///   `gemini-2.5-pro@in>200000`). The highest exceeded threshold wins.
/// - `<model>@batch` / `<model>@flex` — alternate rates for batch / flex (priority) calls.
#[derive(Debug, Clone, Default)]
pub struct PriceBook {
    entries: HashMap<String, ModelPrice>,
    aliases: AliasTable,
}

/// Shape of `config/pricing.json`.
#[derive(Debug, Deserialize)]
struct PricingFile {
    models: HashMap<String, ModelPrice>,
}

impl PriceBook {
    pub fn new(entries: HashMap<String, ModelPrice>) -> Self {
        let aliases = AliasTable::from_pairs(entries.iter().flat_map(|(key, price)| {
            let canonical = key
                .split_once('/')
                .map_or(key.as_str(), |(_, m)| m)
                .to_string();
            price
                .aliases
                .iter()
                .map(move |a| (a.clone(), canonical.clone()))
        }));
        Self { entries, aliases }
    }

    /// The declared alias table this book carries — step 5 of
    /// [`crate::model_id::canonicalize_with`], and what the collective normalizes identities with.
    pub fn aliases(&self) -> &AliasTable {
        &self.aliases
    }

    /// Attach a declared alias table (from the seed) to a book built from DB rows.
    pub fn with_aliases(mut self, aliases: AliasTable) -> Self {
        self.aliases = aliases;
        self
    }

    /// Canonicalize `(provider, model)` through the one algorithm, including this book's aliases.
    pub fn canonical(&self, provider: &str, model: &str) -> ModelId {
        canonicalize_with(provider, model, &self.aliases)
    }

    /// Parse the on-disk `pricing.json` (the `{ "models": { ... } }` form).
    pub fn from_json_str(s: &str) -> Result<Self> {
        let parsed: PricingFile =
            serde_json::from_str(s).map_err(|e| LtError::InvalidPriceBook(e.to_string()))?;
        Ok(Self::new(parsed.models))
    }

    /// The storage key for a `(provider, model)` pair — **the raw strings**, so the key a `PUT
    /// /v1/prices/mistral/x` writes is the key an event from `mistral` reads. (It used to format a
    /// closed enum, which is how every unmodeled provider's rows became unreachable.)
    pub fn key(provider: &str, model: &str) -> String {
        format!("{provider}/{model}")
    }

    /// Build a price book from persisted rows (keyed `"<provider>/<model>"`).
    pub fn from_rows(rows: &[ModelPriceRow]) -> Self {
        let entries = rows
            .iter()
            .map(|r| {
                (
                    format!("{}/{}", r.provider, r.model),
                    ModelPrice {
                        input_per_mtok: r.input_per_mtok,
                        output_per_mtok: r.output_per_mtok,
                        cached_input_per_mtok: r.cached_input_per_mtok,
                        aliases: Vec::new(),
                    },
                )
            })
            .collect();
        // Rows carry no alias column (M8 changes no schema), so a book built from the store has only
        // the derivable canonicalization until [`PriceBook::with_aliases`] re-attaches the seed's.
        Self {
            entries,
            aliases: AliasTable::default(),
        }
    }

    /// Flatten this book into rows (for seeding the DB from `pricing.json`).
    pub fn rows(&self) -> Vec<ModelPriceRow> {
        self.entries
            .iter()
            .filter_map(|(k, v)| {
                let (provider, model) = k.split_once('/')?;
                Some(ModelPriceRow {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    input_per_mtok: v.input_per_mtok,
                    output_per_mtok: v.output_per_mtok,
                    cached_input_per_mtok: v.cached_input_per_mtok,
                    effective_date: Utc::now(),
                    source_url: None,
                })
            })
            .collect()
    }

    /// The `(provider, model)` keys this identity may be priced under, in resolution order: exactly
    /// as written, then canonicalized (provider synonym, `provider/` prefix, lane, date suffix),
    /// then through the declared alias table. The raw pair comes first so a row an operator wrote by
    /// hand always wins over anything we derived.
    fn candidates(&self, provider: &str, model: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::with_capacity(3);
        let mut push = |p: &str, m: &str| {
            let pair = (p.to_string(), m.to_string());
            if !out.contains(&pair) {
                out.push(pair);
            }
        };
        push(provider, model);
        let id = crate::model_id::canonicalize(provider, model);
        push(id.provider.as_str(), &id.family);
        let aliased = id.with_aliases(&self.aliases);
        push(aliased.provider.as_str(), &aliased.family);
        // Last resort: a *classifiable* id falls back to its family's rows, so an OTel span from
        // `az.ai.openai` or `gcp.gemini` is still priced from `openai/…` / `google/…` instead of
        // silently costing nothing. Pricing only — the stored id, the limit scope and the rollup key
        // all stay the raw vendor, which is the whole point of the open vocabulary.
        let family = aliased.provider.family();
        if family.is_known() {
            push(family.as_str(), &aliased.family);
        }
        out
    }

    /// Look up a price for a `(provider, model)` identity, walking [`PriceBook::candidates`].
    pub fn lookup(&self, provider: &str, model: &str) -> Option<&ModelPrice> {
        self.candidates(provider, model)
            .iter()
            .find_map(|(p, m)| self.entries.get(&Self::key(p, m)))
    }

    /// Compute cost in USD at standard rates (convenience for [`PriceBook::cost_usd_mode`]).
    pub fn cost_usd(&self, provider: &str, model: &str, usage: &TokenUsage) -> Option<f64> {
        self.cost_usd_mode(provider, model, usage, PricingMode::Standard)
    }

    /// Compute cost in USD, honoring prompt-length **tiers** and **batch/flex** rates (encoded as
    /// price-row variants — see [`PriceBook`]). `None` if the model is unpriced. Cached input tokens
    /// are billed at the cached rate when one exists; otherwise at the input rate.
    pub fn cost_usd_mode(
        &self,
        provider: &str,
        model: &str,
        usage: &TokenUsage,
        mode: PricingMode,
    ) -> Option<f64> {
        let p = self.resolve(provider, model, usage.input, mode)?;
        let cached = usage.cached_input.unwrap_or(0);
        let billable_input = usage.input.saturating_sub(cached);

        let mut cost = (billable_input as f64) * p.input_per_mtok / 1_000_000.0
            + (usage.output as f64) * p.output_per_mtok / 1_000_000.0;

        let cached_rate = p.cached_input_per_mtok.unwrap_or(p.input_per_mtok);
        cost += (cached as f64) * cached_rate / 1_000_000.0;

        Some(cost)
    }

    /// Resolve the applicable price row for `(provider, model)` given the input size and mode,
    /// walking the same [`PriceBook::candidates`] chain as [`PriceBook::lookup`].
    fn resolve(
        &self,
        provider: &str,
        model: &str,
        input_tokens: u64,
        mode: PricingMode,
    ) -> Option<&ModelPrice> {
        // A lane written into the model name (`gpt-4o@batch`) is a lane, not a model: honor it when
        // the caller didn't already ask for one.
        let lane = crate::model_id::canonicalize(provider, model).lane;
        let mode = match (&lane, mode) {
            (Some(l), PricingMode::Standard) => PricingMode::parse(l),
            _ => mode,
        };
        self.candidates(provider, model)
            .iter()
            .find_map(|(p, m)| self.resolve_exact(p, m, input_tokens, mode))
    }

    fn resolve_exact(
        &self,
        provider: &str,
        model: &str,
        input_tokens: u64,
        mode: PricingMode,
    ) -> Option<&ModelPrice> {
        // A mode-specific variant (e.g. batch rate) wins when present; else fall through to standard.
        if let Some(suffix) = mode.suffix() {
            if let Some(p) = self
                .entries
                .get(&Self::key(provider, &format!("{model}{suffix}")))
            {
                return Some(p);
            }
        }
        // Prompt-length tier: the highest `@in>N` whose threshold is exceeded by the input.
        let prefix = format!("{provider}/{model}@in>");
        let mut best: Option<(u64, &ModelPrice)> = None;
        for (k, v) in &self.entries {
            if let Some(n) = k.strip_prefix(&prefix).and_then(|s| s.parse::<u64>().ok()) {
                if input_tokens > n && best.is_none_or(|(b, _)| n > b) {
                    best = Some((n, v));
                }
            }
        }
        if let Some((_, p)) = best {
            return Some(p);
        }
        // Base rate.
        self.entries.get(&Self::key(provider, model))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> PriceBook {
        let mut m = HashMap::new();
        m.insert(
            "anthropic/claude-haiku-4-5".to_string(),
            ModelPrice {
                input_per_mtok: 1.0,
                output_per_mtok: 5.0,
                cached_input_per_mtok: Some(0.1),
                aliases: Vec::new(),
            },
        );
        PriceBook::new(m)
    }

    #[test]
    fn computes_cost_with_cache() {
        let b = book();
        let usage = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cached_input: Some(500_000),
            reasoning: None,
        };
        // billable input 500k @1.0 = 0.5, cached 500k @0.1 = 0.05, output 1M @5.0 = 5.0
        let c = b.cost_usd("anthropic", "claude-haiku-4-5", &usage).unwrap();
        assert!((c - 5.55).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn date_suffix_fallback() {
        let b = book();
        assert!(b.lookup("anthropic", "claude-haiku-4-5-20251001").is_some());
    }

    #[test]
    fn unknown_model_is_none() {
        assert!(book()
            .cost_usd("openai", "nope", &TokenUsage::default())
            .is_none());
    }

    fn variant_book() -> PriceBook {
        let r = |i, o| ModelPrice {
            input_per_mtok: i,
            output_per_mtok: o,
            cached_input_per_mtok: None,
            aliases: Vec::new(),
        };
        let mut m = HashMap::new();
        m.insert("google/gemini-2.5-pro".to_string(), r(1.25, 10.0)); // <=200k
        m.insert("google/gemini-2.5-pro@in>200000".to_string(), r(2.5, 15.0)); // >200k
        m.insert("openai/gpt-4o".to_string(), r(2.5, 10.0));
        m.insert("openai/gpt-4o@batch".to_string(), r(1.25, 5.0));
        PriceBook::new(m)
    }

    fn usage(input: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input,
            output,
            cached_input: None,
            reasoning: None,
        }
    }

    /// The seed shipped with the source tree, so the two tests below measure the real book.
    const SEED: &str = include_str!("../../../config/pricing.json");

    #[test]
    fn every_declared_alias_points_at_a_priced_model() {
        // The old `model_aliases.json` had 7 of 8 targets absent from the price book, so the table
        // "normalized" identities onto models nothing could cost. Declaring aliases in the seed makes
        // that checkable — and checked.
        let book = PriceBook::from_json_str(SEED).expect("seed parses");
        let priced: Vec<String> = book.rows().into_iter().map(|r| r.model).collect();
        assert!(!book.aliases().is_empty(), "the seed declares aliases");
        for target in book.aliases().targets() {
            assert!(
                priced.iter().any(|m| m == target),
                "alias target {target:?} is not a priced model"
            );
        }
    }

    #[test]
    fn an_unmodeled_provider_prices_from_its_own_row() {
        // The M8 headline: a `mistral/*` row is reachable by a `mistral` event. Under the closed
        // enum the event keyed `unknown/…` and the row keyed `mistral/…`, forever.
        let book = PriceBook::from_json_str(SEED).expect("seed parses");
        let u = usage(1_000_000, 0);
        assert!(book.cost_usd("mistral", "mistral-large", &u).is_some());
        assert!(book.cost_usd("deepseek", "deepseek-chat", &u).is_some());
        assert!(book.cost_usd("groq", "llama-3.3-70b", &u).is_some());
        // …including a hand-written row for a provider the seed never heard of.
        let rows = vec![ModelPriceRow {
            provider: "cerebras".into(),
            model: "zoo-1".into(),
            input_per_mtok: 1.0,
            output_per_mtok: 1.0,
            cached_input_per_mtok: None,
            effective_date: Utc::now(),
            source_url: None,
        }];
        let hand = PriceBook::from_rows(&rows);
        assert_eq!(hand.cost_usd("Cerebras", "ZOO-1", &u), Some(1.0));
    }

    #[test]
    fn declared_aliases_and_dates_resolve_to_the_priced_row() {
        let book = PriceBook::from_json_str(SEED).expect("seed parses");
        let u = usage(1_000_000, 0);
        let base = book.cost_usd("google", "gemini-2.5-pro", &u).unwrap();
        // Declared alias…
        assert_eq!(
            book.cost_usd("google", "gemini-2.5-pro-002", &u),
            Some(base)
        );
        // …provider synonym…
        assert_eq!(
            book.cost_usd("google-vertex", "gemini-2.5-pro", &u),
            Some(base)
        );
        // …a namespaced OTel vendor id, which prices from its family's rows while staying itself…
        assert_eq!(
            book.cost_usd("az.ai.openai", "gpt-4o-mini", &u),
            book.cost_usd("openai", "gpt-4o-mini", &u)
        );
        // …and a dated point release, with no table entry for either date spelling.
        assert_eq!(
            book.cost_usd("anthropic", "claude-haiku-4-5-20251001", &u),
            book.cost_usd("anthropic", "claude-haiku-4-5", &u)
        );
    }

    #[test]
    fn prompt_length_tier() {
        let b = variant_book();
        // 100k input → base rate 1.25/Mtok
        let lo = b
            .cost_usd("google", "gemini-2.5-pro", &usage(100_000, 0))
            .unwrap();
        assert!((lo - 100_000.0 * 1.25 / 1e6).abs() < 1e-12, "got {lo}");
        // 300k input → long-context rate 2.5/Mtok
        let hi = b
            .cost_usd("google", "gemini-2.5-pro", &usage(300_000, 0))
            .unwrap();
        assert!((hi - 300_000.0 * 2.5 / 1e6).abs() < 1e-12, "got {hi}");
    }

    #[test]
    fn batch_variant_and_fallback() {
        let b = variant_book();
        let u = usage(1_000_000, 1_000_000);
        // batch mode → @batch row (1.25 in + 5.0 out)
        let batch = b
            .cost_usd_mode("openai", "gpt-4o", &u, PricingMode::Batch)
            .unwrap();
        assert!((batch - 6.25).abs() < 1e-9, "got {batch}");
        // standard → base (2.5 + 10.0)
        let std = b
            .cost_usd_mode("openai", "gpt-4o", &u, PricingMode::Standard)
            .unwrap();
        assert!((std - 12.5).abs() < 1e-9, "got {std}");
        // flex has no @flex row → falls back to standard base
        let flex = b
            .cost_usd_mode("openai", "gpt-4o", &u, PricingMode::Flex)
            .unwrap();
        assert!((flex - 12.5).abs() < 1e-9, "got {flex}");
    }
}
