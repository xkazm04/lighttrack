//! The rollup vocabulary: what usage can be grouped or filtered by, and which timestamp a window
//! is measured on.
//!
//! [`Dimension`] is **the** vocabulary. It used to be triplicated — `LimitScope::kind_str`, the
//! `dim: &str` arguments, and a SQL whitelist per backend — so a dimension could exist in one place
//! and silently not in another, and the query that read it would group on `NULL`.

use serde::Serialize;

/// How a dimension is stored on an event row. Backends translate this to their own dialect: a
/// column is a column everywhere, a metadata key is `json_extract(metadata,'$.k')` on SQLite,
/// `(NULLIF(metadata,'')::jsonb)->>'k'` on Postgres, and a parsed JSON field on Firestore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    /// A first-class column on `events`.
    Column(&'static str),
    /// A key inside the JSON `metadata` blob (the key name only — no `$.` prefix, no quoting).
    MetadataKey(&'static str),
    /// The UTC calendar day of the query's [`TimeKey`], i.e. the `YYYY-MM-DD` prefix of the
    /// fixed-width RFC3339 timestamp.
    Day,
}

/// The vocabulary of things usage can be grouped or filtered by — **the** vocabulary. It used to be
/// triplicated (`LimitScope::kind_str`, the `dim: &str` arguments, and a SQL whitelist), so a
/// dimension could exist in one place and silently not in another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    Project,
    Provider,
    Model,
    /// Use-case label (`LlmEvent::name`).
    Name,
    /// The id of the API key that wrote the event (server-stamped `metadata.api_key_id`). Never the
    /// key material — see [`crate::LimitScope::ApiKey`].
    ApiKey,
    /// Billing customer (`metadata.customer_id`).
    Customer,
    /// Billing product (`metadata.product_id`).
    Product,
    /// Prompt tag (`metadata.prompt`, `"<name>@v<version>"`).
    Prompt,
    /// UTC calendar day of the query's time key.
    Day,
}

impl Dimension {
    /// Every dimension, in presentation order.
    pub const ALL: &'static [Dimension] = &[
        Dimension::Project,
        Dimension::Provider,
        Dimension::Model,
        Dimension::Name,
        Dimension::ApiKey,
        Dimension::Customer,
        Dimension::Product,
        Dimension::Prompt,
        Dimension::Day,
    ];

    /// The stable wire/storage name (matching the `Serialize` impl and the old `dim` strings).
    pub fn as_str(self) -> &'static str {
        match self {
            Dimension::Project => "project",
            Dimension::Provider => "provider",
            Dimension::Model => "model",
            Dimension::Name => "name",
            Dimension::ApiKey => "api_key",
            Dimension::Customer => "customer",
            Dimension::Product => "product",
            Dimension::Prompt => "prompt",
            Dimension::Day => "day",
        }
    }

    /// Parse a wire name. `None` for anything not in the vocabulary — callers must refuse rather
    /// than fall back, because a "product" query silently answered with customer data is wrong data
    /// presented as right.
    pub fn parse(s: &str) -> Option<Dimension> {
        Dimension::ALL.iter().copied().find(|d| d.as_str() == s)
    }

    /// Where this dimension's value lives on an event row.
    pub fn storage(self) -> Storage {
        match self {
            Dimension::Project => Storage::Column("project_id"),
            Dimension::Provider => Storage::Column("provider"),
            Dimension::Model => Storage::Column("model"),
            Dimension::Name => Storage::Column("name"),
            Dimension::ApiKey => Storage::MetadataKey("api_key_id"),
            Dimension::Customer => Storage::MetadataKey("customer_id"),
            Dimension::Product => Storage::MetadataKey("product_id"),
            Dimension::Prompt => Storage::MetadataKey("prompt"),
            Dimension::Day => Storage::Day,
        }
    }
}

/// Which timestamp the window and the day bucket are measured on.
///
/// Accounting reads key on **`ReceivedAt`** (server arrival): a caller able to slide its spend out
/// of a window by backdating its own events is a caller with no cap. `Ts` is the client-declared
/// event time, correct for "what happened when" reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeKey {
    /// Client-supplied event time.
    #[default]
    Ts,
    /// Server arrival time (falls back to `ts` on rows written before the column existed).
    ReceivedAt,
}

impl TimeKey {
    pub fn as_str(self) -> &'static str {
        match self {
            TimeKey::Ts => "ts",
            TimeKey::ReceivedAt => "received_at",
        }
    }

    /// Parse a wire name; `None` for an unknown key.
    pub fn parse(s: &str) -> Option<TimeKey> {
        match s {
            "ts" => Some(TimeKey::Ts),
            "received_at" => Some(TimeKey::ReceivedAt),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vocabulary_round_trips_and_refuses_anything_else() {
        for d in Dimension::ALL {
            assert_eq!(Dimension::parse(d.as_str()), Some(*d));
        }
        assert_eq!(Dimension::parse("customer_id"), None);
        assert_eq!(Dimension::parse("'; DROP TABLE events; --"), None);
    }

    /// Every dimension resolves to storage, so a variant added without teaching the backends where
    /// to read it cannot compile away into a `NULL` that matches nothing.
    #[test]
    fn every_dimension_has_a_storage_location() {
        for d in Dimension::ALL {
            match d.storage() {
                Storage::Column(c) => assert!(!c.is_empty()),
                Storage::MetadataKey(k) => assert!(!k.is_empty() && !k.starts_with('$')),
                Storage::Day => assert_eq!(*d, Dimension::Day),
            }
        }
    }

    #[test]
    fn time_key_round_trips() {
        assert_eq!(TimeKey::parse("ts"), Some(TimeKey::Ts));
        assert_eq!(TimeKey::parse("received_at"), Some(TimeKey::ReceivedAt));
        assert_eq!(TimeKey::parse("whenever"), None);
        assert_eq!(TimeKey::default(), TimeKey::Ts);
    }
}
