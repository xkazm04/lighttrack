//! `WHERE`-clause construction for the event reads.
//!
//! The filtered listing and the two rollups all accumulate `$n` placeholders in bind order. Building
//! that inline inside three `async fn`s made the generated SQL and its placeholder numbering
//! untestable without a live database; as free functions over an accumulator it is both shared and
//! pinned by the unit tests below.
//!
//! Values are **always bound**, never interpolated — the accumulator only ever pushes a `$n`
//! reference into the SQL text.

use chrono::{DateTime, Utc};

use lighttrack_store::codec::decode_event_cursor;
use lighttrack_store::{EventFilter, Result, StoreError};

use crate::util::fmt_ts;

/// Conditions plus the values they reference, numbered `$1..$n` in push order.
#[derive(Default)]
pub(crate) struct Conds {
    conds: Vec<String>,
    binds: Vec<String>,
}

impl Conds {
    /// Bind `value` and add `"{col} {op} $n"`.
    fn push(&mut self, col: &str, op: &str, value: String) {
        let n = self.bind(value);
        self.conds.push(format!("{col} {op} ${n}"));
    }

    /// Bind `value` without emitting a condition, returning its `$n` index so a caller can write a
    /// compound predicate that mentions it (the keyset cursor references two).
    fn bind(&mut self, value: String) -> usize {
        self.binds.push(value);
        self.binds.len()
    }

    fn push_raw(&mut self, cond: String) {
        self.conds.push(cond);
    }

    /// `""` when there is nothing to filter on, else `"WHERE a AND b "` — the trailing space lets
    /// callers concatenate the next keyword (`ORDER BY` / `GROUP BY`) straight onto it.
    pub(crate) fn where_clause(&self) -> String {
        if self.conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {} ", self.conds.join(" AND "))
        }
    }

    pub(crate) fn binds(&self) -> &[String] {
        &self.binds
    }

    /// How many values are bound, i.e. the highest `$n` used so far. A caller appending its own
    /// placeholder (the `LIMIT`) numbers it `bind_count() + 1`.
    pub(crate) fn bind_count(&self) -> usize {
        self.binds.len()
    }
}

/// Project scope plus an optional `[since, until)` window. Bounds compare against the fixed-width
/// `ts` string, which is chronological as a string sort.
pub(crate) fn window_conds(
    project: Option<&str>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Conds {
    let mut c = Conds::default();
    if let Some(p) = project {
        c.push("project_id", "=", p.to_string());
    }
    if let Some(s) = since {
        c.push("ts", ">=", fmt_ts(s));
    }
    if let Some(u) = until {
        c.push("ts", "<", fmt_ts(u));
    }
    c
}

/// The full predicate for a filtered listing: window, the equality dimensions, and the keyset cursor.
pub(crate) fn list_conds(project: Option<&str>, filter: &EventFilter) -> Result<Conds> {
    let mut c = window_conds(project, filter.since, filter.until);
    for (col, v) in [
        ("provider", &filter.provider),
        ("model", &filter.model),
        ("trace_id", &filter.trace_id),
        ("name", &filter.name),
    ] {
        if let Some(v) = v {
            c.push(col, "=", v.clone());
        }
    }
    if let Some(cursor) = &filter.cursor {
        let (cts, cid) = decode_event_cursor(cursor)
            .ok_or_else(|| StoreError::Other(format!("invalid cursor {cursor:?}")))?;
        let i = c.bind(cts);
        let j = c.bind(cid);
        // Strictly after (cts, cid) in DESC (ts, id) order.
        c.push_raw(format!("(ts < ${i} OR (ts = ${i} AND id < ${j}))"));
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_store::codec::encode_event_cursor;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn no_filter_yields_no_where_clause() {
        let c = window_conds(None, None, None);
        assert_eq!(c.where_clause(), "");
        assert_eq!(c.bind_count(), 0);
    }

    /// Placeholder numbers follow bind order, and the window is half-open: `>=` since, `<` until.
    #[test]
    fn window_numbers_placeholders_in_bind_order() {
        let c = window_conds(
            Some("p1"),
            Some(ts("2026-01-01T00:00:00Z")),
            Some(ts("2026-02-01T00:00:00Z")),
        );
        assert_eq!(
            c.where_clause(),
            "WHERE project_id = $1 AND ts >= $2 AND ts < $3 "
        );
        assert_eq!(c.binds()[0], "p1");
        assert_eq!(c.binds()[1], "2026-01-01T00:00:00.000000000Z");
        assert_eq!(c.binds()[2], "2026-02-01T00:00:00.000000000Z");
    }

    /// A skipped optional does not leave a gap: `until` alone is `$2`, not `$3`.
    #[test]
    fn skipped_bounds_do_not_leave_placeholder_gaps() {
        let c = window_conds(Some("p1"), None, Some(ts("2026-02-01T00:00:00Z")));
        assert_eq!(c.where_clause(), "WHERE project_id = $1 AND ts < $2 ");
        assert_eq!(c.bind_count(), 2);
    }

    /// The keyset predicate must reference the *cursor timestamp* twice ($n) and the id once ($n+1),
    /// in strict-after DESC order — get this wrong and pagination either loops or skips a page.
    #[test]
    fn cursor_binds_both_halves_and_compares_strictly_after() {
        let filter = EventFilter {
            cursor: Some(encode_event_cursor(
                "2026-01-01T00:00:00.000000000Z",
                "ev-9",
            )),
            ..Default::default()
        };
        let c = list_conds(Some("p1"), &filter).expect("cursor decodes");
        assert_eq!(
            c.where_clause(),
            "WHERE project_id = $1 AND (ts < $2 OR (ts = $2 AND id < $3)) "
        );
        assert_eq!(c.binds()[1], "2026-01-01T00:00:00.000000000Z");
        assert_eq!(c.binds()[2], "ev-9");
    }

    #[test]
    fn a_corrupt_cursor_is_an_error_not_a_silent_full_listing() {
        let filter = EventFilter {
            cursor: Some("not-a-cursor!!".to_string()),
            ..Default::default()
        };
        assert!(list_conds(None, &filter).is_err());
    }

    /// Filter values reach the query as bound parameters only. A value carrying SQL must never turn
    /// up in the clause text.
    #[test]
    fn dimension_values_are_bound_never_interpolated() {
        let filter = EventFilter {
            provider: Some("openai".to_string()),
            model: Some("'; DROP TABLE events; --".to_string()),
            ..Default::default()
        };
        let c = list_conds(None, &filter).expect("no cursor");
        assert_eq!(c.where_clause(), "WHERE provider = $1 AND model = $2 ");
        assert!(!c.where_clause().contains("DROP TABLE"));
        assert_eq!(c.binds()[1], "'; DROP TABLE events; --");
    }

    /// The equality dimensions are appended after the window, so a listing that uses both numbers
    /// them window-first — the order `list` binds them in.
    #[test]
    fn dimensions_follow_the_window_in_bind_order() {
        let filter = EventFilter {
            since: Some(ts("2026-01-01T00:00:00Z")),
            trace_id: Some("t-1".to_string()),
            name: Some("summarize".to_string()),
            ..Default::default()
        };
        let c = list_conds(Some("p1"), &filter).expect("no cursor");
        assert_eq!(
            c.where_clause(),
            "WHERE project_id = $1 AND ts >= $2 AND trace_id = $3 AND name = $4 "
        );
    }
}
