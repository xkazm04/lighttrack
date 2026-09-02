//! Which traffic a rule applies to: the event dimensions a scope is matched against, and the scope
//! itself (`None` on a rule = project-wide).

use serde::{Deserialize, Serialize};

use crate::rollup::Dimension;

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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// The rollup [`Dimension`] this scope selects on. Scopes are a *subset* of the rollup
    /// vocabulary, not a parallel one: routing through it is what keeps a cap, a usage breakdown and
    /// a margin query talking about the same column.
    pub fn dimension(&self) -> Dimension {
        match self {
            LimitScope::Provider(_) => Dimension::Provider,
            LimitScope::Model(_) => Dimension::Model,
            LimitScope::Name(_) => Dimension::Name,
            LimitScope::ApiKey(_) => Dimension::ApiKey,
            LimitScope::Customer(_) => Dimension::Customer,
        }
    }

    /// The storage discriminant (`provider` | `model` | `name` | `api_key` | `customer`) — the
    /// dimension's own name, so the two vocabularies cannot drift.
    pub fn kind_str(&self) -> &'static str {
        self.dimension().as_str()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The scope vocabulary is the rollup vocabulary, restricted. A variant naming a dimension that
    /// did not exist would group the breakdown behind it on `NULL` — a cap that never fires and a
    /// "who is spending" page on which nobody is.
    #[test]
    fn every_scope_kind_is_a_rollup_dimension() {
        for kind in LimitScope::KINDS {
            assert!(
                Dimension::parse(kind).is_some(),
                "no dimension for '{kind}'"
            );
        }
        for s in [
            LimitScope::Provider("a".into()),
            LimitScope::Model("a".into()),
            LimitScope::Name("a".into()),
            LimitScope::ApiKey("a".into()),
            LimitScope::Customer("a".into()),
        ] {
            assert_eq!(s.kind_str(), s.dimension().as_str());
            assert!(LimitScope::KINDS.contains(&s.kind_str()));
            assert_eq!(LimitScope::from_parts(s.kind_str(), "a".into()), Some(s));
        }
    }
}
