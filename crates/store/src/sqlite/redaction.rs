//! The redaction posture report: what the ingest boundary did to the rows already in this database.
//!
//! One grouped read over the server-owned `metadata.redaction` object. Grouping on the stored JSON
//! text is exact rather than approximate because the stamp is written by one serializer with a fixed
//! field order (`core::RedactionStamp`), never by a client — the ingest path strips whatever a caller
//! sent under the key first.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};

use lighttrack_core::RedactionStamp;

use crate::codec::fmt_ts;
use crate::{RedactionPostureRow, Result};

/// Events received at or after `since`, grouped by the redaction stamp they carry, most events
/// first.
///
/// Windowed on `received_at` (server-stamped) rather than `ts`: this answers "what did *we* do to
/// what we accepted", and a client that backdates `ts` must not be able to move its rows out of the
/// operator's posture report. `received_at` is also the column `idx_events_project_received` covers.
pub(super) fn posture(
    conn: &Connection,
    project: Option<&str>,
    since: DateTime<Utc>,
) -> Result<Vec<RedactionPostureRow>> {
    let sql = format!(
        "SELECT json_extract(metadata, '$.redaction') AS stamp, COUNT(*) AS n \
         FROM events WHERE {proj} AND received_at >= ?2 \
         GROUP BY stamp ORDER BY n DESC",
        proj = super::project_pred(project),
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![project, fmt_ts(since)], map_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(raws.into_iter().map(from_raw).collect())
}

fn map_row(row: &Row) -> rusqlite::Result<(Option<String>, i64)> {
    Ok((row.get(0)?, row.get(1)?))
}

/// A stamp that will not parse degrades to `None` — the "we do not know" bucket — rather than
/// erroring the whole report. That is the honest reading: an unreadable stamp tells us nothing about
/// the row, which is exactly what `None` already means here.
fn from_raw((stamp, n): (Option<String>, i64)) -> RedactionPostureRow {
    RedactionPostureRow {
        stamp: stamp
            .as_deref()
            .and_then(|j| serde_json::from_str::<RedactionStamp>(j).ok()),
        events: n.max(0) as u64,
    }
}

#[cfg(test)]
mod tests {
    use lighttrack_core::{LlmEvent, Redaction};
    use serde_json::json;

    use super::*;
    use crate::codec::parse_ts;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        super::super::schema::apply(&c).unwrap();
        c
    }

    fn ev(id: &str, redaction: serde_json::Value) -> LlmEvent {
        let mut meta = json!({});
        if !redaction.is_null() {
            meta = json!({ "redaction": redaction });
        }
        serde_json::from_value(json!({
            "id": id, "project_id": "p1", "provider": "anthropic", "model": "m",
            "ts": "2026-06-10T00:00:00Z", "metadata": meta
        }))
        .unwrap()
    }

    /// The three postures an operator must be able to tell apart stay three rows: unstamped,
    /// stamped-but-not-scrubbed, and scrubbed — and identical stamps collapse into one row.
    #[test]
    fn posture_groups_by_stamp_and_keeps_unstamped_separate() {
        let c = conn();
        let scrubbed = json!({ "policy": "none", "scrub": true, "spans": 2, "rules": "abc123" });
        for e in [
            ev("e1", json!(null)),
            ev(
                "e2",
                json!({ "policy": "none", "scrub": false, "spans": 0, "rules": "" }),
            ),
            ev("e3", scrubbed.clone()),
            ev("e4", scrubbed),
        ] {
            super::super::events::insert(&c, &e).unwrap();
        }
        let since = parse_ts("2026-01-01T00:00:00Z").unwrap();
        let rows = posture(&c, Some("p1"), since).unwrap();
        assert_eq!(rows.len(), 3, "three distinct postures: {rows:?}");

        // Most events first: the two identical scrubbed rows collapsed into one group of 2.
        assert_eq!(rows[0].events, 2);
        let top = rows[0].stamp.as_ref().expect("scrubbed group is stamped");
        assert!(top.scrub);
        assert_eq!(top.spans, 2);
        assert_eq!(top.rules, "abc123");
        assert_eq!(top.policy, Redaction::None);

        let unknown = rows
            .iter()
            .find(|r| r.stamp.is_none())
            .expect("the unstamped bucket exists on its own");
        assert_eq!(unknown.events, 1);
        let verbatim = rows
            .iter()
            .find(|r| r.stamp.as_ref().is_some_and(|s| !s.scrub))
            .expect("a deliberate no-scrub is NOT folded in with the unknowns");
        assert_eq!(verbatim.events, 1);
    }

    /// The two event predicates: pick out rows scrubbed by one rule-set generation, and rows the
    /// scrubber actually rewrote. `min_redacted_spans: 0` must still mean "everything", including
    /// unstamped rows — a NULL JSON path that dropped them would answer a narrower question than
    /// the one asked.
    #[test]
    fn the_redaction_predicates_select_by_rule_set_and_by_spans() {
        let c = conn();
        for e in [
            ev("plain", json!(null)),
            ev(
                "old",
                json!({ "policy": "none", "scrub": true, "spans": 1, "rules": "old000000000" }),
            ),
            ev(
                "new1",
                json!({ "policy": "none", "scrub": true, "spans": 5, "rules": "new000000000" }),
            ),
            ev(
                "new0",
                json!({ "policy": "none", "scrub": true, "spans": 0, "rules": "new000000000" }),
            ),
        ] {
            super::super::events::insert(&c, &e).unwrap();
        }
        let page = |f: crate::EventFilter| {
            super::super::events::list_filtered(&c, Some("p1"), &f, 50)
                .unwrap()
                .events
                .into_iter()
                .map(|e| e.id)
                .collect::<std::collections::BTreeSet<_>>()
        };

        let by_rules = page(crate::EventFilter {
            redaction_rules: Some("new000000000".into()),
            ..Default::default()
        });
        assert_eq!(
            by_rules.len(),
            2,
            "both rows from the new rule set: {by_rules:?}"
        );
        assert!(by_rules.contains("new1") && by_rules.contains("new0"));

        let rewritten = page(crate::EventFilter {
            min_redacted_spans: Some(1),
            ..Default::default()
        });
        assert_eq!(rewritten.len(), 2, "only rows the scrub actually rewrote");
        assert!(rewritten.contains("old") && rewritten.contains("new1"));

        let everything = page(crate::EventFilter {
            min_redacted_spans: Some(0),
            ..Default::default()
        });
        assert_eq!(
            everything.len(),
            4,
            "zero spans includes unstamped rows, not excludes them"
        );

        // AND, not OR: the new rule set *and* a rewrite is one row.
        let both = page(crate::EventFilter {
            redaction_rules: Some("new000000000".into()),
            min_redacted_spans: Some(1),
            ..Default::default()
        });
        assert_eq!(both.len(), 1);
        assert!(both.contains("new1"));
    }

    /// The window is on `received_at`, so a client backdating `ts` cannot hide its rows from the
    /// posture report.
    #[test]
    fn the_window_reads_server_stamped_arrival_not_client_event_time() {
        let c = conn();
        let mut backdated = ev(
            "old",
            json!({ "policy": "none", "scrub": true, "spans": 9 }),
        );
        backdated.ts = parse_ts("2001-01-01T00:00:00Z").unwrap();
        super::super::events::insert(&c, &backdated).unwrap();

        let rows = posture(&c, Some("p1"), Utc::now() - chrono::Duration::hours(1)).unwrap();
        assert_eq!(rows.len(), 1, "the backdated row is still in the window");
        assert_eq!(rows[0].events, 1);
    }
}
