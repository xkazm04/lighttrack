use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{LtError, Result};
use crate::event::{Provider, TokenUsage};

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
}

/// Shape of `config/pricing.json`.
#[derive(Debug, Deserialize)]
struct PricingFile {
    models: HashMap<String, ModelPrice>,
}

impl PriceBook {
    pub fn new(entries: HashMap<String, ModelPrice>) -> Self {
        Self { entries }
    }

    /// Parse the on-disk `pricing.json` (the `{ "models": { ... } }` form).
    pub fn from_json_str(s: &str) -> Result<Self> {
        let parsed: PricingFile =
            serde_json::from_str(s).map_err(|e| LtError::InvalidPriceBook(e.to_string()))?;
        Ok(Self::new(parsed.models))
    }

    pub fn key(provider: Provider, model: &str) -> String {
        format!("{}/{}", provider.as_str(), model)
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
                    },
                )
            })
            .collect();
        Self { entries }
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

    /// Look up a price, trying an exact `provider/model` match first, then a date-suffix-trimmed
    /// fallback (e.g. `claude-haiku-4-5-20251001` → `claude-haiku-4-5`).
    pub fn lookup(&self, provider: Provider, model: &str) -> Option<&ModelPrice> {
        if let Some(p) = self.entries.get(&Self::key(provider, model)) {
            return Some(p);
        }
        let trimmed = trim_date_suffix(model);
        if trimmed != model {
            return self.entries.get(&Self::key(provider, trimmed));
        }
        None
    }

    /// Compute cost in USD at standard rates (convenience for [`PriceBook::cost_usd_mode`]).
    pub fn cost_usd(&self, provider: Provider, model: &str, usage: &TokenUsage) -> Option<f64> {
        self.cost_usd_mode(provider, model, usage, PricingMode::Standard)
    }

    /// Compute cost in USD, honoring prompt-length **tiers** and **batch/flex** rates (encoded as
    /// price-row variants — see [`PriceBook`]). `None` if the model is unpriced. Cached input tokens
    /// are billed at the cached rate when one exists; otherwise at the input rate.
    pub fn cost_usd_mode(
        &self,
        provider: Provider,
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
    /// applying the same date-suffix fallback as [`PriceBook::lookup`].
    fn resolve(
        &self,
        provider: Provider,
        model: &str,
        input_tokens: u64,
        mode: PricingMode,
    ) -> Option<&ModelPrice> {
        if let Some(p) = self.resolve_exact(provider, model, input_tokens, mode) {
            return Some(p);
        }
        let trimmed = trim_date_suffix(model);
        if trimmed != model {
            return self.resolve_exact(provider, trimmed, input_tokens, mode);
        }
        None
    }

    fn resolve_exact(
        &self,
        provider: Provider,
        model: &str,
        input_tokens: u64,
        mode: PricingMode,
    ) -> Option<&ModelPrice> {
        // A mode-specific variant (e.g. batch rate) wins when present; else fall through to standard.
        if let Some(suffix) = mode.suffix() {
            if let Some(p) = self.entries.get(&Self::key(provider, &format!("{model}{suffix}"))) {
                return Some(p);
            }
        }
        // Prompt-length tier: the highest `@in>N` whose threshold is exceeded by the input.
        let prefix = format!("{}/{}@in>", provider.as_str(), model);
        let mut best: Option<(u64, &ModelPrice)> = None;
        for (k, v) in &self.entries {
            if let Some(n) = k.strip_prefix(&prefix).and_then(|s| s.parse::<u64>().ok()) {
                if input_tokens > n && best.map_or(true, |(b, _)| n > b) {
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

/// Strip a trailing `-YYYYMMDD` date suffix if present.
fn trim_date_suffix(model: &str) -> &str {
    if let Some((head, tail)) = model.rsplit_once('-') {
        if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
            return head;
        }
    }
    model
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
        let c = b.cost_usd(Provider::Anthropic, "claude-haiku-4-5", &usage).unwrap();
        assert!((c - 5.55).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn date_suffix_fallback() {
        let b = book();
        assert!(b
            .lookup(Provider::Anthropic, "claude-haiku-4-5-20251001")
            .is_some());
    }

    #[test]
    fn unknown_model_is_none() {
        assert!(book().cost_usd(Provider::OpenAi, "nope", &TokenUsage::default()).is_none());
    }

    fn variant_book() -> PriceBook {
        let r = |i, o| ModelPrice { input_per_mtok: i, output_per_mtok: o, cached_input_per_mtok: None };
        let mut m = HashMap::new();
        m.insert("google/gemini-2.5-pro".to_string(), r(1.25, 10.0)); // <=200k
        m.insert("google/gemini-2.5-pro@in>200000".to_string(), r(2.5, 15.0)); // >200k
        m.insert("openai/gpt-4o".to_string(), r(2.5, 10.0));
        m.insert("openai/gpt-4o@batch".to_string(), r(1.25, 5.0));
        PriceBook::new(m)
    }

    fn usage(input: u64, output: u64) -> TokenUsage {
        TokenUsage { input, output, cached_input: None, reasoning: None }
    }

    #[test]
    fn prompt_length_tier() {
        let b = variant_book();
        // 100k input → base rate 1.25/Mtok
        let lo = b.cost_usd(Provider::Google, "gemini-2.5-pro", &usage(100_000, 0)).unwrap();
        assert!((lo - 100_000.0 * 1.25 / 1e6).abs() < 1e-12, "got {lo}");
        // 300k input → long-context rate 2.5/Mtok
        let hi = b.cost_usd(Provider::Google, "gemini-2.5-pro", &usage(300_000, 0)).unwrap();
        assert!((hi - 300_000.0 * 2.5 / 1e6).abs() < 1e-12, "got {hi}");
    }

    #[test]
    fn batch_variant_and_fallback() {
        let b = variant_book();
        let u = usage(1_000_000, 1_000_000);
        // batch mode → @batch row (1.25 in + 5.0 out)
        let batch = b.cost_usd_mode(Provider::OpenAi, "gpt-4o", &u, PricingMode::Batch).unwrap();
        assert!((batch - 6.25).abs() < 1e-9, "got {batch}");
        // standard → base (2.5 + 10.0)
        let std = b.cost_usd_mode(Provider::OpenAi, "gpt-4o", &u, PricingMode::Standard).unwrap();
        assert!((std - 12.5).abs() < 1e-9, "got {std}");
        // flex has no @flex row → falls back to standard base
        let flex = b.cost_usd_mode(Provider::OpenAi, "gpt-4o", &u, PricingMode::Flex).unwrap();
        assert!((flex - 12.5).abs() < 1e-9, "got {flex}");
    }
}
