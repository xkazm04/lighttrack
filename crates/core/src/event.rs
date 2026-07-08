use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::pricing::{PriceBook, PricingMode};

/// LLM provider. `Unknown` captures anything we don't model yet (its pricing lookups miss → `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    OpenAi,
    Anthropic,
    Google,
    #[serde(other)]
    Unknown,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::OpenAi => "openai",
            Provider::Anthropic => "anthropic",
            Provider::Google => "google",
            Provider::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Default for Provider {
    fn default() -> Self {
        Provider::Unknown
    }
}

/// The kind of operation. `Other` catches anything unmodeled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    Chat,
    Completion,
    Embedding,
    #[serde(other)]
    Other,
}

impl Operation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Operation::Chat => "chat",
            Operation::Completion => "completion",
            Operation::Embedding => "embedding",
            Operation::Other => "other",
        }
    }
}

impl Default for Operation {
    fn default() -> Self {
        Operation::Chat
    }
}

/// Call outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Success,
    Error,
    Timeout,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Success => "success",
            Status::Error => "error",
            Status::Timeout => "timeout",
        }
    }
}

impl Default for Status {
    fn default() -> Self {
        Status::Success
    }
}

/// Token accounting for a single call. `cached_input`/`reasoning` are optional and provider-dependent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input + self.output
    }
}

/// One normalized LLM call — the canonical record everything else is derived from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmEvent {
    #[serde(default = "crate::new_id")]
    pub id: String,
    /// Defaulted so keyed ingest can omit it (the API derives it from the API key).
    #[serde(default)]
    pub project_id: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,

    #[serde(default = "Utc::now")]
    pub ts: DateTime<Utc>,
    pub provider: Provider,
    pub model: String,
    /// Optional use-case / call-site name (e.g. "summarize-email"). The unit the
    /// Personas "LLM Overview" rollup groups by; falls back to provider+model when
    /// absent. Set it per call via the SDK (`track(..., name=...)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub operation: Operation,

    #[serde(default)]
    pub usage: TokenUsage,

    /// Provider-reported cost if known; otherwise filled by [`LlmEvent::ensure_cost`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(default)]
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,

    /// Optional, redactable payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
}

impl LlmEvent {
    /// If no provider-reported cost is set, compute it from the price book (best effort), honoring
    /// the call's pricing lane (batch/flex) and prompt-length tiers. Returns the resolved cost.
    pub fn ensure_cost(&mut self, prices: &PriceBook) -> Option<f64> {
        if self.cost_usd.is_none() {
            let mode = self.pricing_mode();
            self.cost_usd = prices.cost_usd_mode(self.provider, &self.model, &self.usage, mode);
        }
        self.cost_usd
    }

    /// Billing customer this call is attributed to, read from `metadata.customer_id`. The linkage
    /// rides in `metadata` (not a column) so it stays backward-compatible across every store backend;
    /// margin rollups group on it. `None` when the SDK didn't tag the call.
    pub fn customer_id(&self) -> Option<&str> {
        self.metadata.get("customer_id").and_then(Value::as_str)
    }

    /// Billing product/feature this call is attributed to, read from `metadata.product_id`.
    pub fn product_id(&self) -> Option<&str> {
        self.metadata.get("product_id").and_then(Value::as_str)
    }

    /// The pricing lane for this call: an explicit `metadata.pricing_mode`, else a `batch` / `flex`
    /// (or `priority`) tag, else standard.
    fn pricing_mode(&self) -> PricingMode {
        if let Some(m) = self.metadata.get("pricing_mode").and_then(Value::as_str) {
            return PricingMode::parse(m);
        }
        if self.tags.iter().any(|t| t == "batch") {
            return PricingMode::Batch;
        }
        if self.tags.iter().any(|t| t == "flex" || t == "priority") {
            return PricingMode::Flex;
        }
        PricingMode::Standard
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn ev(metadata: Value) -> LlmEvent {
        serde_json::from_value(json!({
            "provider": "anthropic", "model": "claude-haiku-4-5", "metadata": metadata
        }))
        .unwrap()
    }

    #[test]
    fn billing_ids_read_from_metadata() {
        let e = ev(json!({ "customer_id": "cus_123", "product_id": "chat" }));
        assert_eq!(e.customer_id(), Some("cus_123"));
        assert_eq!(e.product_id(), Some("chat"));
    }

    #[test]
    fn billing_ids_absent_when_untagged() {
        assert_eq!(ev(Value::Null).customer_id(), None);
        assert_eq!(ev(json!({ "other": 1 })).product_id(), None);
    }
}
