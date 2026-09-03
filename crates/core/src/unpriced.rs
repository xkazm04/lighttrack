//! The unpriced-traffic ledger: which `(provider, model)` pairs the price book has never heard of,
//! and how much traffic each of them is quietly reporting as `$0.00`.
//!
//! The null-cost invariant — a call we cannot price stores `cost_usd = NULL`, never a zero — has
//! been honoured at ingest for a long time, and disclosed on traces and inside limit evaluation.
//! What was missing is the operator's half of the loop: nothing listed *what* was unpriced, so the
//! only way to find out was to notice a cost dashboard that felt low. This is that list, and it is
//! ranked by call count because the top row is the price worth adding first.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One unpriced `(provider, model)` pair over a window.
///
/// `first_seen` / `last_seen` are **UTC day** granularity: the ledger is folded out of the M2
/// grouped rollup (`group_by [provider, model, day]`, unpriced rows only), which is the one query
/// every backend already implements. A day is precise enough to answer "is this still happening?" —
/// the question the field exists for — and buying more precision would mean a second per-backend
/// query and a fourth place for the unpriced predicate to drift.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnpricedRow {
    pub provider: String,
    pub model: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// First UTC day in the window with unpriced traffic on this key.
    pub first_seen: DateTime<Utc>,
    /// Last UTC day in the window with unpriced traffic on this key.
    pub last_seen: DateTime<Utc>,
}

impl UnpricedRow {
    pub fn tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// What `GET /v1/costs/unpriced` answers: the ledger plus the pointer an operator needs to act.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnpricedLedger {
    /// Window start the ledger was measured over.
    pub since: DateTime<Utc>,
    /// Unpriced pairs, ranked by `calls` descending — the first row is the price worth adding first.
    pub models: Vec<UnpricedRow>,
    /// Total unpriced calls in the window, across every row.
    pub unpriced_calls: u64,
    /// How to close each row, spelled out rather than left to the reader: every cost number over
    /// this window is a **floor** until these are priced.
    pub notes: &'static str,
}

/// The note every unpriced ledger carries. One string, so the CLI, the MCP tool and the API agree
/// on what the operator is being told to do.
pub const UNPRICED_NOTES: &str = "Each row is traffic stored with cost_usd = NULL: counted, never \
     costed. Every cost, margin and limit number over this window is a FLOOR until these are \
     priced. Add a rate with PUT /v1/prices/{provider}/{model} — append ?fill_unpriced=1 to price \
     the historical rows for that key at the same time.";

impl UnpricedLedger {
    /// Rank `models` by calls (descending, then by key for a stable order) and total them up.
    pub fn new(since: DateTime<Utc>, mut models: Vec<UnpricedRow>) -> Self {
        models.sort_by(|a, b| {
            b.calls
                .cmp(&a.calls)
                .then_with(|| (&a.provider, &a.model).cmp(&(&b.provider, &b.model)))
        });
        let unpriced_calls = models.iter().map(|r| r.calls).sum();
        UnpricedLedger {
            since,
            models,
            unpriced_calls,
            notes: UNPRICED_NOTES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(model: &str, calls: u64) -> UnpricedRow {
        UnpricedRow {
            provider: "acme".into(),
            model: model.into(),
            calls,
            input_tokens: 10,
            output_tokens: 5,
            first_seen: Utc::now(),
            last_seen: Utc::now(),
        }
    }

    #[test]
    fn the_ledger_ranks_by_calls_and_totals_them() {
        let l = UnpricedLedger::new(Utc::now(), vec![row("b", 2), row("a", 9), row("c", 2)]);
        assert_eq!(
            l.models
                .iter()
                .map(|r| r.model.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"],
            "loudest first, then a stable key order"
        );
        assert_eq!(l.unpriced_calls, 13);
        assert_eq!(l.models[0].tokens(), 15);
    }

    #[test]
    fn an_empty_ledger_is_a_zero_not_a_silence() {
        let l = UnpricedLedger::new(Utc::now(), Vec::new());
        assert_eq!(l.unpriced_calls, 0);
        assert!(l.notes.contains("FLOOR"));
    }
}
