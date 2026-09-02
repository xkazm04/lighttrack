//! Forward fill: price the rows a missing rate left at `cost_usd IS NULL`.
//!
//! Deliberately narrow. Only NULL-cost rows for one `(provider, model)` are eligible, the rate comes
//! from the caller's book through the *same* [`PriceBook::cost_usd_mode`] ingest uses (so tiers and
//! batch/flex lanes resolve identically), and every row written is stamped
//! `metadata.cost_source = "book_fill"`. A row already costed — the caller's own number, or the book
//! at ingest — is never touched, which is the `no-retroactive-repricing` rule holding: a fill closes
//! a gap, it does not restate history.
//!
//! Paged, one transaction per page: a fill can span millions of rows, and holding SQLite's single
//! write connection for all of them would stall ingest for the whole backfill.

use rusqlite::{params, Connection};
use serde_json::Value;

use lighttrack_core::{PricingMode, TokenUsage};

use crate::pricing::PriceFill;
use crate::Result;

/// One stored row a fill is considering.
struct Candidate {
    id: String,
    usage: TokenUsage,
    tags: Vec<String>,
    metadata: Value,
}

pub(super) fn fill(conn: &Connection, f: &PriceFill<'_>) -> Result<u64> {
    let page = f.page.max(1);
    let mut filled: u64 = 0;
    loop {
        let batch = candidates(conn, f, page)?;
        if batch.is_empty() {
            break;
        }
        let n = batch.len();
        let wrote = write_page(conn, f, batch)?;
        filled += wrote;
        // A page that priced nothing would loop forever on the same rows: the book cannot price this
        // key after all (a fill aimed at a variant that leaves the base model unpriced), and leaving
        // the NULLs alone is the honest outcome. Stop rather than spin.
        if wrote == 0 || n < page {
            break;
        }
    }
    Ok(filled)
}

/// The next page of unpriced rows for this key. Re-queried each page rather than paged by offset:
/// each transaction removes its rows from the predicate, so "the first `page` NULL-cost rows" is
/// always the next page, and no cursor can be invalidated by concurrent ingest.
fn candidates(conn: &Connection, f: &PriceFill<'_>, page: usize) -> Result<Vec<Candidate>> {
    let mut stmt = conn.prepare(
        "SELECT id, input_tokens, output_tokens, cached_input_tokens, reasoning_tokens, \
                tags, metadata \
           FROM events \
          WHERE cost_usd IS NULL AND provider = ?1 AND model = ?2 \
          LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(params![f.provider, f.model, page as i64], |r| {
            Ok(Candidate {
                id: r.get(0)?,
                usage: TokenUsage {
                    input: r.get::<_, i64>(1)?.max(0) as u64,
                    output: r.get::<_, i64>(2)?.max(0) as u64,
                    cached_input: r.get::<_, Option<i64>>(3)?.map(|v| v.max(0) as u64),
                    reasoning: r.get::<_, Option<i64>>(4)?.map(|v| v.max(0) as u64),
                },
                tags: json_or_default(r.get::<_, Option<String>>(5)?),
                metadata: r
                    .get::<_, Option<String>>(6)?
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or(Value::Null),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn json_or_default(raw: Option<String>) -> Vec<String> {
    raw.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Price and write one page in a single transaction.
fn write_page(conn: &Connection, f: &PriceFill<'_>, batch: Vec<Candidate>) -> Result<u64> {
    // Sound for the same reason `revenue::insert_batch` gives: the caller already holds the write
    // connection's mutex, so no other statement can interleave inside this transaction.
    let tx = conn.unchecked_transaction()?;
    let mut wrote = 0u64;
    {
        let mut up = tx.prepare(
            "UPDATE events SET cost_usd = ?1, metadata = ?2 WHERE id = ?3 AND cost_usd IS NULL",
        )?;
        for mut c in batch {
            let mode = PricingMode::from_hints(&c.metadata, &c.tags);
            let Some(cost) = f.cost_for(&c.usage, mode) else {
                continue;
            };
            f.stamp(&mut c.metadata);
            wrote += up.execute(params![cost, c.metadata.to_string(), c.id])? as u64;
        }
    }
    tx.commit()?;
    Ok(wrote)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_core::{LlmEvent, ModelPrice, PriceBook};
    use serde_json::json;
    use std::collections::HashMap;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().expect("in-memory db");
        c.execute_batch(include_str!("../../../../schema/sqlite/001_init.sql"))
            .expect("schema");
        c
    }

    fn book() -> PriceBook {
        let mut m = HashMap::new();
        m.insert(
            "acme/zoo-1".to_string(),
            ModelPrice {
                input_per_mtok: 2.0,
                output_per_mtok: 4.0,
                cached_input_per_mtok: None,
                aliases: Vec::new(),
            },
        );
        m.insert(
            "acme/zoo-1@batch".to_string(),
            ModelPrice {
                input_per_mtok: 1.0,
                output_per_mtok: 2.0,
                cached_input_per_mtok: None,
                aliases: Vec::new(),
            },
        );
        PriceBook::new(m)
    }

    fn ev(id: &str, model: &str, cost: Option<f64>, meta: Value) -> LlmEvent {
        let mut e: LlmEvent = serde_json::from_value(json!({
            "id": id, "project_id": "p1", "provider": "acme", "model": model,
            "ts": "2026-06-10T01:00:00Z",
            "usage": { "input": 1000000, "output": 1000000 }, "metadata": meta,
        }))
        .expect("fixture");
        e.cost_usd = cost;
        e.received_at = e.ts;
        e
    }

    fn cost_of(c: &Connection, id: &str) -> Option<f64> {
        c.query_row("SELECT cost_usd FROM events WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .expect("row")
    }

    fn source_of(c: &Connection, id: &str) -> Option<String> {
        c.query_row(
            "SELECT json_extract(metadata,'$.cost_source') FROM events WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .expect("row")
    }

    #[test]
    fn a_fill_prices_only_the_null_rows_and_is_idempotent() {
        let c = conn();
        for e in [
            ev("null-std", "zoo-1", None, json!({"customer_id": "acme"})),
            ev(
                "null-batch",
                "zoo-1",
                None,
                json!({"pricing_mode": "batch"}),
            ),
            // Already costed by the caller: untouchable, whatever the new rate says.
            ev(
                "client",
                "zoo-1",
                Some(99.0),
                json!({"cost_source": "client"}),
            ),
            // A different model: out of scope for this key's fill.
            ev("other", "zoo-2", None, json!({})),
        ] {
            super::super::events::insert(&c, &e).expect("insert");
        }

        let b = book();
        let mut f = PriceFill::new("acme", "zoo-1", &b);
        f.page = 1; // exercise the paging loop
        let n = fill(&c, &f).expect("fill");
        assert_eq!(n, 2, "both NULL rows for this key, and nothing else");

        assert_eq!(cost_of(&c, "null-std"), Some(6.0), "1M in @2 + 1M out @4");
        assert_eq!(
            cost_of(&c, "null-batch"),
            Some(3.0),
            "the lane on the row still chooses the @batch rate"
        );
        assert_eq!(source_of(&c, "null-std").as_deref(), Some("book_fill"));
        assert_eq!(
            c.query_row(
                "SELECT json_extract(metadata,'$.customer_id') FROM events WHERE id = 'null-std'",
                [],
                |r| r.get::<_, Option<String>>(0)
            )
            .expect("row")
            .as_deref(),
            Some("acme"),
            "the caller's own metadata survives the stamp"
        );

        assert_eq!(cost_of(&c, "client"), Some(99.0), "never repriced");
        assert_eq!(source_of(&c, "client").as_deref(), Some("client"));
        assert_eq!(cost_of(&c, "other"), None, "a different key is untouched");

        // The idempotency the conformance suite pins: nothing is left to fill.
        assert_eq!(fill(&c, &f).expect("second fill"), 0);
    }

    /// A book that still cannot price the key must leave the NULLs alone — and must not spin.
    #[test]
    fn an_unpriceable_key_fills_nothing_and_terminates() {
        let c = conn();
        super::super::events::insert(&c, &ev("a", "zoo-9", None, json!({}))).expect("insert");
        let b = book();
        let f = PriceFill::new("acme", "zoo-9", &b);
        assert_eq!(fill(&c, &f).expect("fill"), 0);
        assert_eq!(cost_of(&c, "a"), None, "an honest NULL, not a wrong zero");
    }
}
