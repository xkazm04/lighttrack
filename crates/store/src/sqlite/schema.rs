//! Schema creation + the additive migration list, applied on every open.
//!
//! There's no migration runner here: the schema batch is `CREATE ... IF NOT EXISTS` (which skips
//! existing tables), so columns added after a database was first created are `ALTER`ed in and only
//! "duplicate column name" — i.e. already applied — counts as success. Anything else re-raises.
//!
//! Extracted from [`super`] so `SqliteStore::open` can migrate a fresh connection *before* the
//! read-only pool is opened against it: pooled readers must never see a pre-migration schema, and a
//! read-only connection could not apply the migrations itself.

use rusqlite::Connection;

use crate::Result;

pub(super) const SCHEMA: &str = include_str!("../../../../schema/sqlite/001_init.sql");

/// Columns added after the original schema shipped. Each is `ALTER`ed into a *pre-existing* table;
/// on a fresh database the table doesn't exist yet and the `CREATE TABLE` in [`SCHEMA`] defines the
/// column directly, so "no such table" is as much a success here as "duplicate column name".
const ADDED_COLUMNS: &[&str] = &[
    // DBs created before `events.name` existed.
    "ALTER TABLE events ADD COLUMN name TEXT",
    // Additive migrations for limit rules created before the soft-warning tier and dimension
    // scoping existed.
    "ALTER TABLE limit_rules ADD COLUMN warn_at REAL",
    "ALTER TABLE limit_rules ADD COLUMN scope_kind TEXT",
    "ALTER TABLE limit_rules ADD COLUMN scope_value TEXT",
    // Collective digest v2: per-bucket quality variance (for merged CIs), plus the coarse judge
    // family and rubric-shape fingerprint that scored the bucket.
    "ALTER TABLE collective_entries ADD COLUMN quality_variance REAL",
    "ALTER TABLE collective_entries ADD COLUMN judge_provider TEXT",
    "ALTER TABLE collective_entries ADD COLUMN rubric_fingerprint TEXT",
    // Collective digest v3: benchmark rigor — how reproducible the contributing runs were, whether
    // their cases came from one frozen pin, and whether their verdicts were significance-tested.
    "ALTER TABLE collective_entries ADD COLUMN determinism TEXT",
    "ALTER TABLE collective_entries ADD COLUMN frozen_dataset TEXT",
    "ALTER TABLE collective_entries ADD COLUMN significance_tested TEXT",
    // Collective consent: per-project opt-in to digest contribution (default off).
    "ALTER TABLE projects ADD COLUMN collective_opt_in INTEGER NOT NULL DEFAULT 0",
    // Verdict provenance: structured judge detail (core::ScoreDetail) as JSON.
    "ALTER TABLE scores ADD COLUMN detail TEXT",
    // Run-scoped case results: which benchmark run produced this verdict, and where in its dataset.
    // `idx_scores_run` in [`SCHEMA`] indexes both, so these ALTERs MUST run before the batch —
    // see the ordering note in [`apply`].
    "ALTER TABLE scores ADD COLUMN run_id TEXT",
    "ALTER TABLE scores ADD COLUMN case_index INTEGER",
    // Honest failure accounting on the job queue: `failures` (runs that actually failed — the retry
    // budget) apart from `attempts` (claims, which a crash also burns), and `stale_reclaims` (worker
    // deaths). Both are read by the claim/finish statements, so a database created before they
    // existed must be widened here, before the batch.
    "ALTER TABLE jobs ADD COLUMN failures INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE jobs ADD COLUMN stale_reclaims INTEGER NOT NULL DEFAULT 0",
];

/// Server-stamped arrival time, kept apart from [`ADDED_COLUMNS`] because it needs a backfill.
const ADD_RECEIVED_AT: &str = "ALTER TABLE events ADD COLUMN received_at TEXT";

pub(super) fn apply(c: &Connection) -> Result<()> {
    // Columns FIRST, batch second. [`SCHEMA`] indexes some of these columns
    // (`idx_events_project_received`), and an index over a column an old table doesn't have yet is a
    // hard error — so on a database predating the column the batch would fail before the `ALTER`
    // could rescue it. Widening the tables first makes the two steps order-independent.
    for stmt in ADDED_COLUMNS {
        add_column(c, stmt)?;
    }
    // The backfill (`received_at = ts`) runs ONLY when the ALTER just succeeded, so an
    // already-migrated database never pays a full-table UPDATE on every startup, and pre-existing
    // rows stay valid (their arrival time is their event time — the best information the old schema
    // carried).
    let backfill = add_column(c, ADD_RECEIVED_AT)?;
    c.execute_batch(SCHEMA)?;
    if backfill {
        c.execute("UPDATE events SET received_at = ts WHERE received_at IS NULL", [])?;
    }
    Ok(())
}

/// Run one `ADD COLUMN`, reporting whether it actually widened an existing table. "Already applied"
/// and "table not created yet" are both fine; anything else re-raises.
fn add_column(c: &Connection, stmt: &str) -> Result<bool> {
    match c.execute(stmt, []) {
        Ok(_) => Ok(true),
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
