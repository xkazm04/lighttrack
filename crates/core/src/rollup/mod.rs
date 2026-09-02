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

mod dimension;

use serde::Serialize;

use chrono::{DateTime, Utc};

pub use dimension::{Dimension, Storage, TimeKey};

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
    /// Restrict to rows with **no price on them** (`cost_usd IS NULL`) — the unpriced-traffic
    /// ledger (M26).
    ///
    /// Not expressible as a [`Dimension`] filter: "unpriced" is a property of the cost column, not
    /// a groupable value, and `RollupRow::unpriced_calls` alone cannot answer it because the token
    /// sums beside it cover the whole bucket, priced calls included. One flag on the one rollup
    /// keeps the predicate in a single place per backend instead of a fourth hand-written query.
    pub unpriced_only: bool,
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
            unpriced_only: false,
        }
    }

    /// Narrow to calls with no price on the row — see [`RollupQuery::unpriced_only`].
    pub fn only_unpriced(mut self) -> Self {
        self.unpriced_only = true;
        self
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
}
