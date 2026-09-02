//! The ordered statement plan each dialect applies, derived from [`super::tables`].
//!
//! One ordering rule, and it is guaranteed by construction rather than by remembering it:
//! **CREATE TABLEs, then ADD COLUMNs, then the raw steps, then CREATE INDEXes.** An index over a
//! column added after its table shipped is the classic way a schema batch dies on an existing
//! deployment and passes on the developer's fresh one; putting every widening ahead of every index
//! makes the two order-independent.
//!
//! The list is idempotent on both sides: `CREATE … IF NOT EXISTS` skips what exists, Postgres's
//! `ADD COLUMN IF NOT EXISTS` skips what exists, and SQLite's applier tolerates "duplicate column
//! name" (already applied) and "no such table" (not created yet) as success.

use super::model::Dialect;
use super::render::add_column_stmt;
use super::{render_bq, render_pg, render_sqlite, tables};

/// A migration step that no declarative form can express — a primary-key rewrite, a backfill.
///
/// Kept verbatim rather than modelled. A `DO $$ … $$` block guarded on the key's own column list is
/// exactly the kind of statement that is correct because of how it is written; paraphrasing it
/// through a renderer would be a way to get it subtly wrong for no gain.
#[derive(Debug, Clone, Copy)]
pub struct Raw {
    pub name: &'static str,
    pub sql: &'static str,
}

/// Postgres raw steps, applied after the widenings and before the indexes.
pub const PG_RAW: &[Raw] = &[
    Raw {
        name: "events_received_at_backfill",
        sql: "\
-- Backfill arrival time to event time for pre-migration rows (the reads COALESCE too, so the two
-- agree either way). The partial index makes the backfill index-driven and, once it has run, empty
-- — so re-running the schema batch on every boot costs a lookup instead of a seq scan over events.
CREATE INDEX IF NOT EXISTS idx_events_received_backfill ON events(id) WHERE received_at IS NULL;
UPDATE events SET received_at = ts WHERE received_at IS NULL;",
    },
    Raw {
        name: "m26_dated_price_book",
        sql: "\
-- M26 — the price book becomes a dated, append-only timeline. Not an ADD COLUMN: the identity of a
-- rate has to become (provider, model, effective_from), or a correction overwrites the row that
-- priced last quarter's traffic and no June cost number can ever be defended.
--
-- The pre-M26 date column becomes the key's date. Renamed rather than added beside it, so a row
-- carries exactly one date and nothing has to decide which of two spellings wins.
DO $m26$
BEGIN
  IF EXISTS (SELECT 1 FROM information_schema.columns
              WHERE table_name = 'model_prices' AND column_name = 'effective_date')
     AND NOT EXISTS (SELECT 1 FROM information_schema.columns
              WHERE table_name = 'model_prices' AND column_name = 'effective_from')
  THEN
    ALTER TABLE model_prices RENAME COLUMN effective_date TO effective_from;
  END IF;
END
$m26$;

-- Both nullable on the way in: nobody vouched for a pre-M26 rate, and stamping \"verified today\"
-- onto rows nobody checked would make the staleness warning repeat a lie.
ALTER TABLE model_prices ADD COLUMN IF NOT EXISTS effective_from TEXT;
ALTER TABLE model_prices ADD COLUMN IF NOT EXISTS verified_at TEXT;
ALTER TABLE model_prices ADD COLUMN IF NOT EXISTS note TEXT;
UPDATE model_prices SET effective_from = '1970-01-01T00:00:00.000000000Z'
 WHERE effective_from IS NULL;
ALTER TABLE model_prices ALTER COLUMN effective_from SET NOT NULL;

-- Widen the primary key in place, guarded on the key's own column list so this runs once and is a
-- no-op on every subsequent apply.
DO $m26pk$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_index i
      JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
     WHERE i.indrelid = 'model_prices'::regclass AND i.indisprimary
       AND a.attname = 'effective_from')
  THEN
    ALTER TABLE model_prices DROP CONSTRAINT IF EXISTS model_prices_pkey;
    ALTER TABLE model_prices
      ADD CONSTRAINT model_prices_pkey PRIMARY KEY (provider, model, effective_from);
  END IF;
END
$m26pk$;",
    },
];

/// The raw steps for `d`. SQLite's two (the `model_prices` rebuild and the `received_at` backfill)
/// are **not** here: both are conditional on a shape SQL alone cannot test for, so they live in
/// `crate::sqlite::schema` as named Rust steps that read `PRAGMA table_info` first.
pub fn raw_for(d: Dialect) -> &'static [Raw] {
    match d {
        Dialect::Postgres => PG_RAW,
        _ => &[],
    }
}

/// Every `CREATE TABLE`, in declaration order.
pub fn create_tables(d: Dialect) -> Vec<String> {
    tables::all()
        .iter()
        .map(|t| match d {
            Dialect::Sqlite => render_sqlite::create_table(t),
            Dialect::Postgres => render_pg::create_table(t),
            Dialect::BigQuery => render_bq::create_table(t),
        })
        .collect()
}

/// Every post-ship column widening, table by table, in declaration order.
///
/// This is the list that used to be hand-mirrored in two places: `ADDED_COLUMNS` /
/// `ADDED_COLUMNS_LATE` in the SQLite backend, and the `ALTER TABLE … IF NOT EXISTS` lines strewn
/// through `schema/postgres/001_init.sql`. It is now one projection of the model, so the two
/// dialects cannot disagree about which columns exist.
pub fn add_columns(d: Dialect) -> Vec<String> {
    let mut out = Vec::new();
    for t in tables::all() {
        for c in t.added_columns() {
            out.push(add_column_stmt(t.name, c, d));
        }
    }
    out
}

/// Every `CREATE INDEX` this dialect declares.
pub fn indexes(d: Dialect) -> Vec<String> {
    let mut out = Vec::new();
    for t in tables::all() {
        for i in t.indexes.iter().filter(|i| i.serves(d)) {
            out.push(match d {
                Dialect::Postgres => render_pg::create_index(t, i),
                _ => render_sqlite::create_index(t, i),
            });
        }
    }
    out
}

/// The full ordered plan for `d`, ready to execute statement by statement.
pub fn plan(d: Dialect) -> Vec<String> {
    let mut out = create_tables(d);
    out.extend(add_columns(d));
    out.extend(raw_for(d).iter().map(|r| r.sql.to_string()));
    out.extend(indexes(d));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one invariant the ordering exists for.
    #[test]
    fn every_widening_precedes_every_index() {
        for d in [Dialect::Sqlite, Dialect::Postgres] {
            let plan = plan(d);
            let last_alter = plan
                .iter()
                .rposition(|s| s.starts_with("ALTER TABLE"))
                .expect("the model has post-ship columns");
            let first_index = plan
                .iter()
                .position(|s| s.starts_with("CREATE INDEX") || s.starts_with("CREATE UNIQUE"))
                .expect("the model has indexes");
            assert!(
                last_alter < first_index,
                "{d:?}: an index would be created over a column not yet added"
            );
        }
    }

    /// Every index names a column the model declares — the drift that used to be caught only by a
    /// deployment failing to boot.
    #[test]
    fn every_index_covers_declared_columns() {
        for t in tables::all() {
            for i in t.indexes {
                for raw in i.columns.split(',') {
                    let name = raw.split_whitespace().next().unwrap_or("");
                    assert!(
                        t.column(name).is_some(),
                        "{}: index {} names unknown column {name}",
                        t.name,
                        i.name
                    );
                }
            }
        }
    }
}
