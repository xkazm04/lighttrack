//! Model-derived select lists, usable exactly where a `const COLS: &str` used to be.
//!
//! A backend's per-table `COLS` is not free-form SQL: it is "every column of this table, in wire
//! order", which the model already knows. [`SelectList`] lets a backend say that instead of
//! spelling it out — it builds the string once, on first use, and renders through `Display`, so
//! `format!("SELECT {COLS} FROM events …")` keeps working verbatim.
//!
//! The property this buys is the one the hand-written constants could not have: a column added to
//! the model is *in* the select list, so a positional mapper that was not updated fails loudly on
//! the next column instead of silently reading a neighbour.

use std::fmt;
use std::sync::OnceLock;

pub struct SelectList {
    cell: OnceLock<String>,
    build: fn() -> String,
}

impl SelectList {
    pub const fn new(build: fn() -> String) -> Self {
        Self {
            cell: OnceLock::new(),
            build,
        }
    }

    pub fn as_str(&self) -> &str {
        self.cell.get_or_init(self.build)
    }

    /// How many columns the list names — the arity a positional mapper must agree with.
    ///
    /// Counts top-level commas only: an entry can be an expression (`COALESCE(received_at, ts) AS
    /// received_at`) whose own comma is one column, not two.
    pub fn len(&self) -> usize {
        let mut depth = 0usize;
        let mut n = 1usize;
        for c in self.as_str().chars() {
            match c {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => n += 1,
                _ => {}
            }
        }
        n
    }

    pub fn is_empty(&self) -> bool {
        self.as_str().is_empty()
    }
}

impl fmt::Display for SelectList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{model::Dialect, tables};

    static EVENTS_COLS: SelectList =
        SelectList::new(|| tables::EVENTS.select_list(Dialect::Sqlite));

    /// The three lists M14 replaced, pinned verbatim.
    ///
    /// This is the evidence that deriving them changed *nothing*: each backend's row mapper reads
    /// by position, so if the model's wire order differed from the string it replaced by even one
    /// column, every field after that point would silently be read from its neighbour. Postgres
    /// cannot be checked by running it here — there is no database in this test — so it is checked
    /// by equality with what it has been serving.
    #[test]
    fn the_derived_lists_are_exactly_the_ones_the_backends_shipped() {
        assert_eq!(
            tables::EVENTS.select_list(Dialect::Sqlite),
            "id, project_id, trace_id, span_id, parent_span_id, ts, provider, model, operation, \
             input_tokens, output_tokens, cached_input_tokens, reasoning_tokens, cost_usd, \
             latency_ms, status, error, input, output, tags, source, metadata, name, \
             COALESCE(received_at, ts) AS received_at"
        );
        assert_eq!(
            tables::SCORES.select_list(Dialect::Sqlite),
            "id, project_id, event_id, rubric, value, max, pass, reasoning, detail, run_id, \
             case_index, scored_by, cost_usd, created_at, rubric_id, kind"
        );
        assert_eq!(
            tables::SCORES.select_list(Dialect::Postgres),
            "id, project_id, event_id, rubric, value, \"max\", pass, reasoning, detail, run_id, \
             case_index, scored_by, cost_usd, created_at, rubric_id, kind"
        );
        assert_eq!(
            tables::JOBS.select_list(Dialect::Sqlite),
            "id, type, payload, status, attempts, max_attempts, progress, error, result, \
             claimed_at, created_at, updated_at, failures, stale_reclaims, project_id"
        );
    }

    #[test]
    fn a_select_list_renders_and_counts_its_columns() {
        assert!(EVENTS_COLS.as_str().starts_with("id, project_id, "));
        assert!(EVENTS_COLS
            .as_str()
            .ends_with("COALESCE(received_at, ts) AS received_at"));
        assert_eq!(EVENTS_COLS.len(), tables::EVENTS.columns.len());
        assert_eq!(format!("{EVENTS_COLS}"), EVENTS_COLS.as_str());
    }
}
