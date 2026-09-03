//! Schema creation + the additive migration list, applied on every open — all of it rendered from
//! the declarative model in [`crate::schema`].
//!
//! There is no migration runner and no version counter. The plan is one ordered list of statements
//! ([`crate::schema::plan`]) that is idempotent from either side: `CREATE … IF NOT EXISTS` skips
//! what exists, and an `ADD COLUMN` that reports "duplicate column name" (already applied) or "no
//! such table" (not created yet) counts as success. Anything else re-raises.
//!
//! The ordering that used to need a two-pass dance — the pre-batch `ADDED_COLUMNS` list, the batch,
//! then the same list again plus `ADDED_COLUMNS_LATE` — is now guaranteed by construction: every
//! `CREATE TABLE` precedes every `ADD COLUMN`, which precedes every `CREATE INDEX`. So a fresh
//! database gets its tables and then its post-ship columns, an old one gets only the columns, and
//! no index is ever attempted over a column that is not there yet.
//!
//! Extracted from [`super`] so `SqliteStore::open` can migrate a fresh connection *before* the
//! read-only pool is opened against it: pooled readers must never see a pre-migration schema, and a
//! read-only connection could not apply the migrations itself.

use rusqlite::Connection;

use crate::schema::{model::Dialect, plan};
use crate::Result;

pub(super) fn apply(c: &Connection) -> Result<()> {
    // BEFORE anything creates a table: `auto_vacuum` is a property of the FILE, writable only while
    // the database is still empty. Setting it here makes every database created from 2026-08-24 on
    // able to hand freed pages back to the filesystem in yieldable chunks
    // (`PRAGMA incremental_vacuum(N)`, see [`super::maintenance`]) instead of only ever growing.
    //
    // On an existing database this statement is a documented no-op — SQLite ignores it rather than
    // failing — which is why the mode is READ BACK by the storage report rather than assumed, and
    // why an older file's report says outright that incremental reclamation is unavailable on it and
    // names the offline remedy. Same discipline as `journal_mode`: a pragma that can silently not
    // take effect is not a setting until someone reads the answer.
    //
    // `incremental` and not `full`: `full` vacuums on every commit, which puts reclamation work on
    // the ingest hot path — the opposite of the quiet-window design.
    c.execute_batch("PRAGMA auto_vacuum=INCREMENTAL")?;
    c.execute_batch("PRAGMA journal_mode = WAL")?;

    // The one migration a `CREATE`/`ALTER` cannot express, and it has to run before the batch: on a
    // pre-M26 file `model_prices` exists in the old shape, so the model's `CREATE TABLE IF NOT
    // EXISTS` would be a no-op and leave it there.
    dated_price_book(c)?;

    let mut backfill_received_at = false;
    for stmt in plan(Dialect::Sqlite) {
        let applied = run(c, &stmt)?;
        // The backfill (`received_at = ts`) runs ONLY when the ALTER just succeeded, so an
        // already-migrated database never pays a full-table UPDATE on every startup, and
        // pre-existing rows stay valid — their arrival time is their event time, the best
        // information the old schema carried.
        if applied && stmt == "ALTER TABLE events ADD COLUMN received_at TEXT" {
            backfill_received_at = true;
        }
    }
    if backfill_received_at {
        c.execute(
            "UPDATE events SET received_at = ts WHERE received_at IS NULL",
            [],
        )?;
    }
    Ok(())
}

/// Run one planned statement, reporting whether it actually changed anything.
///
/// "Already applied" and "table not created yet" are both fine; anything else re-raises.
fn run(c: &Connection, stmt: &str) -> Result<bool> {
    match c.execute_batch(stmt) {
        Ok(()) => Ok(true),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("duplicate column name") || msg.contains("no such table") {
                Ok(false)
            } else {
                Err(e.into())
            }
        }
    }
}

/// M26 — turn `model_prices` from one overwritten row per model into a dated, append-only table
/// keyed `(provider, model, effective_from)`.
///
/// This is the one migration in the file that an `ALTER` cannot express: SQLite can add columns but
/// not change a primary key, so the table has to be rebuilt. Detection is by *shape*, not by a
/// version counter — if `effective_from` is already a column the rebuild is skipped — which makes it
/// idempotent on every open, including the very first one on a fresh database (where the table does
/// not exist yet and this is a no-op before the model creates it in its final shape).
///
/// The copy preserves history rather than inventing it: an existing row's `effective_date` becomes
/// its `effective_from`, and `verified_at` stays NULL — nobody vouched for those rates, and writing
/// "verified today" would be a lie the staleness warning would then repeat.
fn dated_price_book(c: &Connection) -> Result<()> {
    if !table_exists(c, "model_prices")? || has_column(c, "model_prices", "effective_from")? {
        return Ok(());
    }
    c.execute_batch(
        "BEGIN;
         CREATE TABLE model_prices_m26 (
           provider              TEXT NOT NULL,
           model                 TEXT NOT NULL,
           input_per_mtok        REAL NOT NULL,
           output_per_mtok       REAL NOT NULL,
           cached_input_per_mtok REAL,
           effective_from        TEXT NOT NULL,
           source_url            TEXT,
           verified_at           TEXT,
           note                  TEXT,
           PRIMARY KEY (provider, model, effective_from)
         );
         INSERT OR IGNORE INTO model_prices_m26
           (provider, model, input_per_mtok, output_per_mtok, cached_input_per_mtok,
            effective_from, source_url, verified_at, note)
           SELECT provider, model, input_per_mtok, output_per_mtok, cached_input_per_mtok,
                  effective_date, source_url, NULL, NULL
             FROM model_prices;
         DROP TABLE model_prices;
         ALTER TABLE model_prices_m26 RENAME TO model_prices;
         COMMIT;",
    )?;
    Ok(())
}

fn table_exists(c: &Connection, table: &str) -> Result<bool> {
    let n: i64 = c.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Whether `table` carries `column`, read off `PRAGMA table_info` — the shape check the rebuild
/// keys on.
fn has_column(c: &Connection, table: &str, column: &str) -> Result<bool> {
    // `PRAGMA table_info` takes no bound parameters; `table` is a compile-time literal at every
    // call site, never caller text.
    let mut stmt = c.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        if r.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applying the plan twice must leave the same schema — the property the whole "no version
    /// counter" design rests on, and the local stand-in for the Postgres idempotency check.
    #[test]
    fn applying_the_plan_twice_is_a_no_op() {
        let c = Connection::open_in_memory().expect("db");
        apply(&c).expect("first apply");
        let first = schema_dump(&c);
        apply(&c).expect("second apply");
        assert_eq!(first, schema_dump(&c));
        assert!(first.len() > 25, "the dump should cover every table");
    }

    fn schema_dump(c: &Connection) -> Vec<String> {
        let mut stmt = c
            .prepare(
                "SELECT type || ' ' || name || ' ' || COALESCE(sql,'') FROM sqlite_master \
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
            )
            .expect("prepare");
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows");
        rows
    }
}
