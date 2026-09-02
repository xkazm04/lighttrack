//! The one grouped-rollup question: "usage and cost over a window, grouped by one to three
//! dimensions".
//!
//! Eight `Store` methods used to ask it, each with its own signature, its own row type, and its own
//! hand-written `GROUP BY`. Four of them existed on SQLite only, so the production Postgres
//! deployment answered `501` for `/v1/forecast` and three margin surfaces — not because the data
//! was missing but because nobody had written the ninth near-identical query. [`RollupQuery`] is
//! the single shape a backend implements once; the legacy methods are adapters over it.
//!
//! Three things are deliberately *in* the row that the legacy DTOs left out:
//!
//! * `unpriced_calls` — calls whose model was absent from the price book, so `cost_usd` is NULL on
//!   the event. Every legacy aggregate summed those as `$0.00` and reported the result as the
//!   spend. A zero you can't distinguish from "we don't know" is the failure this closes.
//! * `client_reported_cost_usd` — the part of the sum that came from a caller's own number
//!   (`metadata.cost_source = "client"`) rather than our price-book arithmetic.
//! * `errors` — failed calls still cost money and still count against a cap.

use serde::Serialize;

use chrono::{DateTime, Utc};

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

/// The most dimensions one rollup may group by. Three is what every caller needs (the widest legacy
/// grouping is `project + provider + model`), and each extra dimension multiplies the row count a
/// backend has to materialize.
pub const MAX_GROUP_BY: usize = 3;

/// One grouped-rollup request.
#[derive(Debug, Clone)]
pub struct RollupQuery<'a> {
    /// Restrict to one project, or `None` for every project (an admin-only read).
    pub project: Option<&'a str>,
    /// 1..=[`MAX_GROUP_BY`] distinct dimensions; the row's `keys` align with this order.
    pub group_by: Vec<Dimension>,
    /// Window start, inclusive.
    pub since: DateTime<Utc>,
    /// Window end, exclusive. `None` means "up to now" (no upper bound).
    pub until: Option<DateTime<Utc>>,
    pub time_key: TimeKey,
    /// Equality predicates, AND-combined. A row whose dimension value is absent never matches
    /// (an untagged call cannot satisfy a customer filter), mirroring [`crate::LimitScope::matches`].
    pub filter: Vec<(Dimension, String)>,
}

impl<'a> RollupQuery<'a> {
    /// A query grouped by `group_by` over `[since, ..)`, keyed on the client `ts`.
    pub fn new(group_by: &[Dimension], since: DateTime<Utc>) -> Self {
        RollupQuery {
            project: None,
            group_by: group_by.to_vec(),
            since,
            until: None,
            time_key: TimeKey::Ts,
            filter: Vec::new(),
        }
    }

    pub fn project(mut self, project: Option<&'a str>) -> Self {
        self.project = project;
        self
    }

    pub fn until(mut self, until: Option<DateTime<Utc>>) -> Self {
        self.until = until;
        self
    }

    pub fn time_key(mut self, k: TimeKey) -> Self {
        self.time_key = k;
        self
    }

    pub fn filter(mut self, dim: Dimension, value: impl Into<String>) -> Self {
        self.filter.push((dim, value.into()));
        self
    }

    /// Why this query cannot be answered as written, or `None` when it is well-formed. Backends
    /// call this first so every one of them refuses the same malformed request identically.
    pub fn invalid(&self) -> Option<String> {
        if self.group_by.is_empty() {
            return Some("a rollup needs at least one group_by dimension".into());
        }
        if self.group_by.len() > MAX_GROUP_BY {
            return Some(format!(
                "a rollup groups by at most {MAX_GROUP_BY} dimensions, got {}",
                self.group_by.len()
            ));
        }
        for (i, d) in self.group_by.iter().enumerate() {
            if self.group_by[..i].contains(d) {
                return Some(format!("duplicate group_by dimension '{}'", d.as_str()));
            }
        }
        if self.filter.iter().any(|(d, _)| *d == Dimension::Day) {
            return Some("the `day` dimension cannot be filtered on; use the window".into());
        }
        if let Some(u) = self.until {
            if u < self.since {
                return Some("window end precedes its start".into());
            }
        }
        None
    }
}

/// One grouped row. `keys[i]` is the value of `group_by[i]`, or `None` where the event carries no
/// value on that dimension (an unnamed call, an untagged customer) — folded into a single bucket
/// rather than dropped, so the parts always sum to the whole.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RollupRow {
    pub keys: Vec<Option<String>>,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The **stored** cost sum: what `SUM(cost_usd)` sees, with unpriced calls contributing nothing.
    /// Read it together with `unpriced_calls` — alone it is an understatement of unknown size.
    pub cost_usd: f64,
    /// Calls in this bucket with no price on the row (`cost_usd IS NULL`).
    pub unpriced_calls: u64,
    /// The part of `cost_usd` the caller self-reported rather than us pricing it.
    pub client_reported_cost_usd: f64,
    /// Calls whose `status` was not `success`. Failures cost money too.
    pub errors: u64,
}

impl RollupRow {
    /// An empty bucket for `keys`, for the client-side folds (Firestore) that accumulate into it.
    pub fn empty(keys: Vec<Option<String>>) -> Self {
        RollupRow {
            keys,
            calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            unpriced_calls: 0,
            client_reported_cost_usd: 0.0,
            errors: 0,
        }
    }

    pub fn tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// The value of `group_by[i]`, or `None` when the query didn't group by that position.
    pub fn key(&self, i: usize) -> Option<&str> {
        self.keys.get(i).and_then(|k| k.as_deref())
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
    fn malformed_queries_are_refused_before_any_sql_is_built() {
        let now = Utc::now();
        let q = RollupQuery::new(&[], now);
        assert!(q.invalid().is_some(), "empty group_by");

        let q = RollupQuery::new(
            &[
                Dimension::Model,
                Dimension::Provider,
                Dimension::Name,
                Dimension::Day,
            ],
            now,
        );
        assert!(q.invalid().is_some(), "too many dimensions");

        let q = RollupQuery::new(&[Dimension::Model, Dimension::Model], now);
        assert!(q.invalid().is_some(), "duplicate dimension");

        let q = RollupQuery::new(&[Dimension::Model], now).filter(Dimension::Day, "2026-01-01");
        assert!(q.invalid().is_some(), "day is a window, not a filter");

        let q =
            RollupQuery::new(&[Dimension::Model], now).until(Some(now - chrono::Duration::days(1)));
        assert!(q.invalid().is_some(), "inverted window");

        let ok = RollupQuery::new(&[Dimension::Provider, Dimension::Model], now)
            .project(Some("p1"))
            .time_key(TimeKey::ReceivedAt)
            .filter(Dimension::Customer, "acme");
        assert_eq!(ok.invalid(), None);
    }

    #[test]
    fn a_row_reads_its_keys_positionally() {
        let r = RollupRow {
            keys: vec![Some("openai".into()), None],
            calls: 3,
            input_tokens: 10,
            output_tokens: 5,
            cost_usd: 1.5,
            unpriced_calls: 1,
            client_reported_cost_usd: 0.5,
            errors: 1,
        };
        assert_eq!(r.key(0), Some("openai"));
        assert_eq!(r.key(1), None, "an untagged bucket, not a missing position");
        assert_eq!(r.key(9), None);
        assert_eq!(r.tokens(), 15);
    }

    #[test]
    fn time_key_round_trips() {
        assert_eq!(TimeKey::parse("ts"), Some(TimeKey::Ts));
        assert_eq!(TimeKey::parse("received_at"), Some(TimeKey::ReceivedAt));
        assert_eq!(TimeKey::parse("whenever"), None);
        assert_eq!(TimeKey::default(), TimeKey::Ts);
    }
}
