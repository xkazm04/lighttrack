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
    // Tenancy lifecycle: what a key may do, when it stops working, and when a project was archived.
    // All three are nullable — an existing row carries no opinion, and `core::decode_scopes` reads
    // that as the permissive back-compat default rather than locking a live key out on upgrade.
    "ALTER TABLE api_keys ADD COLUMN scopes TEXT",
    "ALTER TABLE api_keys ADD COLUMN expires_at TEXT",
    "ALTER TABLE projects ADD COLUMN archived_at TEXT",
];

/// Columns added **after** the batch rather than before it.
///
/// [`ADDED_COLUMNS`] runs first because [`SCHEMA`] indexes some of the columns it adds, and an index
/// over a column an old table lacks is a hard error. These are the opposite case: the table they
/// widen is *defined further up the same file they are appended to*, so on a fresh database a
/// pre-batch `ALTER` finds no table, no-ops, and the `CREATE TABLE` then defines the table without
/// them — the column exists on upgraded databases and is missing on new ones, which is the worst of
/// both. Running them after the batch gives one order that is right for both: the table exists
/// either way, and "duplicate column name" is the success case on a database that already has them.
///
/// Nothing in [`SCHEMA`] may index a column listed here (`idx_relay_lease` keys on `status` +
/// `lease_deadline`, both original).
///
/// M7 — the relay's fenced, renewable lease. `failures` is the retry budget (runs that actually ran
/// and failed) and `stale_reclaims` counts device deaths, kept apart for the same reason the job
/// queue keeps them apart: a device killed mid-run must not consume one of the task's chances.
/// `lease_fence` is the holding device's identity, compared exactly by settle/renew/progress, and
/// `progress` is the liveness detail those writes publish.
const ADDED_COLUMNS_AFTER_SCHEMA: &[&str] = &[
    "ALTER TABLE relay_tasks ADD COLUMN failures INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE relay_tasks ADD COLUMN stale_reclaims INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE relay_tasks ADD COLUMN lease_fence TEXT",
    "ALTER TABLE relay_tasks ADD COLUMN progress TEXT",
];

/// Server-stamped arrival time, kept apart from [`ADDED_COLUMNS`] because it needs a backfill.
const ADD_RECEIVED_AT: &str = "ALTER TABLE events ADD COLUMN received_at TEXT";

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
    for stmt in ADDED_COLUMNS_AFTER_SCHEMA {
        add_column(c, stmt)?;
    }
    if backfill {
        c.execute(
            "UPDATE events SET received_at = ts WHERE received_at IS NULL",
            [],
        )?;
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
