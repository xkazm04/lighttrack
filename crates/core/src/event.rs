use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::pricing::{PriceBook, PricingMode};

/// LLM provider. `Unknown` captures anything we don't model yet (its pricing lookups miss → `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    OpenAi,
    Anthropic,
    Google,
    #[serde(other)]
    #[default]
    Unknown,
}

impl Provider {
    /// Parse a wire/DB provider literal. Anything outside the vocabulary becomes [`Provider::Unknown`]
    /// — the explicit quarantine variant, not a silent coercion into a real provider.
    ///
    /// Exists so callers that hold a provider as a `&str` (a judge spec, a price-book row) reach the
    /// same enum every other path uses, instead of comparing strings and quietly building a second
    /// pricing or attribution vocabulary beside the first.
    pub fn from_wire(s: &str) -> Provider {
        match s.trim().to_ascii_lowercase().as_str() {
            "openai" => Provider::OpenAi,
            "anthropic" => Provider::Anthropic,
            "google" => Provider::Google,
            _ => Provider::Unknown,
        }
    }

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

/// The kind of operation. `Other` catches anything unmodeled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    #[default]
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

/// Call outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Success,
    Error,
    Timeout,
}

impl Status {
    /// Every outcome, so a wire-filter validator can derive its accepted set from the enum rather
    /// than hand-maintaining a parallel string list that drifts when a variant is added.
    pub const ALL: [Status; 3] = [Status::Success, Status::Error, Status::Timeout];

    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Success => "success",
            Status::Error => "error",
            Status::Timeout => "timeout",
        }
    }

    /// Parse a wire literal back to a [`Status`], or `None` when it is outside the vocabulary.
    pub fn from_wire(s: &str) -> Option<Status> {
        Status::ALL.into_iter().find(|v| v.as_str() == s)
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
    /// Server-stamped arrival time — when this API instance accepted the call for accounting.
    ///
    /// Distinct from [`LlmEvent::ts`], which is *client* event time: the client owns it, may backdate
    /// or future-date it, and a single skewed clock keying rolling windows would silently corrupt
    /// budget enforcement. So every windowed accounting read (limit admission, `/v1/limits/status`,
    /// the forecast daily series) keys on `received_at`, while `ts` stays the queryable/orderable
    /// event time users debug with.
    ///
    /// Never read from a request body (`skip_deserializing`) — the server always stamps it — but
    /// always serialized on reads. Rows written before the column existed carry `received_at = ts`
    /// (the migration's backfill).
    #[serde(skip_deserializing, default = "Utc::now")]
    pub received_at: DateTime<Utc>,
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

    /// How this call's `cost_usd` was determined, read from `metadata.cost_source`: `"client"` when
    /// the caller reported it verbatim, `"book"` when we priced it from the DB price book. `None`
    /// when the cost was never resolved (the model is absent from the book — the deliberate
    /// "unpriced means `None`, never a phantom zero" invariant) or the event predates the stamp.
    pub fn cost_source(&self) -> Option<&str> {
        self.metadata.get("cost_source").and_then(Value::as_str)
    }

    /// Whether this call's cost is the client's own number rather than our arithmetic. Limit
    /// evaluation reports the client-reported share so an operator can see when a cap is resting on
    /// self-reported spend.
    pub fn cost_is_client_reported(&self) -> bool {
        self.cost_source() == Some("client")
    }

    /// Billing product/feature this call is attributed to, read from `metadata.product_id`.
    pub fn product_id(&self) -> Option<&str> {
        self.metadata.get("product_id").and_then(Value::as_str)
    }

    /// The **id** of the API key that wrote this call, read from `metadata.api_key_id`.
    ///
    /// Rides in `metadata` (not a column) for the same reason `customer_id` does: every store backend
    /// carries it unchanged, with no cross-backend migration. It is *server-owned* — the ingest path
    /// stamps it from the authenticated principal and strips whatever the client sent — so a caller
    /// cannot forge attribution or dodge a per-key cap by claiming to be another key.
    ///
    /// The value is the opaque `api_keys.id`, never the key material or a hash of it.
    pub fn api_key_id(&self) -> Option<&str> {
        self.metadata.get("api_key_id").and_then(Value::as_str)
    }

    /// The dimensions limit scopes are matched against.
    pub fn scope_dims(&self) -> crate::limits::ScopeDims<'_> {
        crate::limits::ScopeDims {
            provider: self.provider.as_str(),
            model: &self.model,
            name: self.name.as_deref(),
            api_key_id: self.api_key_id(),
            customer_id: self.customer_id(),
        }
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
    use chrono::Datelike;
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
    fn received_at_is_server_owned_and_ignores_the_client() {
        // A client may set `ts` freely, but `received_at` is never taken from the body — otherwise
        // the trust fix would be one JSON field away from being bypassed.
        let e: LlmEvent = serde_json::from_value(json!({
            "provider": "anthropic", "model": "m",
            "ts": "2000-01-01T00:00:00Z",
            "received_at": "2000-01-01T00:00:00Z"
        }))
        .unwrap();
        assert_eq!(e.ts.to_rfc3339(), "2000-01-01T00:00:00+00:00");
        assert!(
            e.received_at.year() > 2020,
            "received_at must be server-stamped, not client-supplied"
        );
        // …and it still round-trips out on reads.
        let v = serde_json::to_value(&e).unwrap();
        assert!(v.get("received_at").is_some());
    }

    #[test]
    fn billing_ids_absent_when_untagged() {
        assert_eq!(ev(Value::Null).customer_id(), None);
        assert_eq!(ev(json!({ "other": 1 })).product_id(), None);
    }
}
