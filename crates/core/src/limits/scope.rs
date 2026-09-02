//! Which traffic a rule applies to: the event dimensions a scope is matched against, and the scope
//! itself (`None` on a rule = project-wide).

use serde::{Deserialize, Serialize};

/// The dimensions of one event that a [`LimitScope`] can be matched against. Passed as a struct
/// rather than a widening tuple of `&str`s so adding a dimension (as `api_key` and `customer` were)
/// doesn't silently re-order every call site's positional arguments.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScopeDims<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    /// Use-case label (`LlmEvent::name`).
    pub name: Option<&'a str>,
    /// The **id** of the API key that wrote the event (`metadata.api_key_id`, server-stamped).
    /// Never the key material or a hash of it — see the note on [`LimitScope::ApiKey`].
    pub api_key_id: Option<&'a str>,
    /// Billing customer (`metadata.customer_id`), the same linkage margin analytics group on.
    pub customer_id: Option<&'a str>,
}

impl<'a> ScopeDims<'a> {
    /// The three original dimensions, for callers with no key/customer context (tests, tools).
    pub fn new(provider: &'a str, model: &'a str, name: Option<&'a str>) -> Self {
        ScopeDims {
            provider,
            model,
            name,
            api_key_id: None,
            customer_id: None,
        }
    }
}

/// Optional dimension a limit is scoped to — provider / model / use-case (`name`) / API key /
/// billing customer. An unscoped rule (`None` on [`LimitRule::scope`]) applies to the whole project,
/// exactly as before; a scoped rule only counts (and can reject) traffic matching the selected
/// dimension value, so an operator can "cap gpt-4o at $5/day", "cap use-case X", or give a staging
/// key $5/day while production keeps $500 — without touching other traffic.
///
/// Serializes externally-tagged, e.g. `{"model":"gpt-4o"}` / `{"provider":"openai"}` /
/// `{"name":"summarize"}` / `{"api_key":"<key-id>"}` / `{"customer":"cus_123"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitScope {
    Provider(String),
    Model(String),
    /// Use-case, matched against an event's `name`.
    Name(String),
    /// One API key, identified by its **row id** — the opaque primary key of the `api_keys` table.
    ///
    /// Deliberately *not* the key material, and not a hash of it: the id is generated independently
    /// of the secret, so nothing about the key can be recovered from a rule, a status payload, or an
    /// alert. The non-secret `prefix` would also have worked, but the id is what the keys API already
    /// returns and what the event carries, so there is exactly one identifier to reason about.
    ApiKey(String),
    /// One billing customer, matched against `metadata.customer_id` — the same linkage margin
    /// analytics already group on.
    Customer(String),
}

impl LimitScope {
    /// The storage discriminant (`provider` | `model` | `name` | `api_key` | `customer`).
    pub fn kind_str(&self) -> &'static str {
        match self {
            LimitScope::Provider(_) => "provider",
            LimitScope::Model(_) => "model",
            LimitScope::Name(_) => "name",
            LimitScope::ApiKey(_) => "api_key",
            LimitScope::Customer(_) => "customer",
        }
    }

    /// The scoped value.
    pub fn value(&self) -> &str {
        match self {
            LimitScope::Provider(v)
            | LimitScope::Model(v)
            | LimitScope::Name(v)
            | LimitScope::ApiKey(v)
            | LimitScope::Customer(v) => v,
        }
    }

    /// Reconstruct from stored `(kind, value)` columns; `None` for an unknown kind.
    pub fn from_parts(kind: &str, value: String) -> Option<LimitScope> {
        match kind {
            "provider" => Some(LimitScope::Provider(value)),
            "model" => Some(LimitScope::Model(value)),
            "name" => Some(LimitScope::Name(value)),
            "api_key" => Some(LimitScope::ApiKey(value)),
            "customer" => Some(LimitScope::Customer(value)),
            _ => None,
        }
    }

    /// Every scope discriminant, for surfaces that enumerate the dimensions (e.g. the per-dimension
    /// usage breakdown). Order is the one they're presented in.
    pub const KINDS: &'static [&'static str] =
        &["provider", "model", "name", "api_key", "customer"];

    /// A compact `kind=value` label for alert messages / dedup keys / rendering.
    pub fn label(&self) -> String {
        format!("{}={}", self.kind_str(), self.value())
    }

    /// Whether an event with these dimensions falls under this scope. A dimension the event doesn't
    /// carry (`None`) never matches a scope on it — an untagged call can't be charged to a customer
    /// cap, exactly as an unnamed call can't be charged to a use-case cap.
    pub fn matches(&self, d: &ScopeDims<'_>) -> bool {
        match self {
            LimitScope::Provider(v) => d.provider == v,
            // Model scopes compare on the **canonical** identity, so a cap on `gpt-4o` also catches
            // `gpt-4o-2024-08-06` and `openai/gpt-4o`. A dated release is the same model for
            // spending purposes, and a cap an operator has to re-state per point release is a cap
            // that silently stops covering traffic the week the vendor ships one.
            LimitScope::Model(v) => {
                d.model == v
                    || crate::model_id::canonicalize(d.provider, d.model).family
                        == crate::model_id::canonicalize(d.provider, v).family
            }
            LimitScope::Name(v) => d.name == Some(v.as_str()),
            LimitScope::ApiKey(v) => d.api_key_id == Some(v.as_str()),
            LimitScope::Customer(v) => d.customer_id == Some(v.as_str()),
        }
    }
}

/// Whether a rule's optional scope admits an event with these dimensions. `None` (unscoped) always
/// matches — identical to pre-scope behavior.
pub fn scope_matches(scope: Option<&LimitScope>, dims: &ScopeDims<'_>) -> bool {
    scope.is_none_or(|s| s.matches(dims))
}
