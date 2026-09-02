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

/// Additive statements applied after the original schema shipped — `ADD COLUMN`, plus the
/// `CREATE INDEX IF NOT EXISTS` that covers a column added here (an index cannot live in [`SCHEMA`]
/// if its column does not, and it cannot precede its own `ALTER`).
///
/// Each runs against a table that may or may not exist yet, so "no such table" is as much a success
/// here as "duplicate column name" — and [`apply`] runs the whole list on **both sides** of the
/// `CREATE` batch, which is what lets a column live here alone instead of also being written into
/// `001_init.sql`.
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
    // FX provenance on a revenue row (M9-B). `amount_usd` is derived and a wrong rate makes it
    // wrong; the provider's minor-unit figure never needs restating, so keeping it — with the rate,
    // the book version that produced it, and whether a real conversion happened — is what makes a
    // rate correction a reprice instead of a re-ingest. All nullable: a pre-M9 row carries no
    // opinion, and `RevenueEvent::is_converted` reads that as "base currency converted, others
    // unknown" rather than flagging every historical USD invoice as approximate.
    "ALTER TABLE revenue_events ADD COLUMN amount_minor INTEGER",
    "ALTER TABLE revenue_events ADD COLUMN fx_rate REAL",
    "ALTER TABLE revenue_events ADD COLUMN fx_book_version TEXT",
    "ALTER TABLE revenue_events ADD COLUMN converted INTEGER",
    // C. Typed verdict identity (M9-C). `scores.rubric` is one free-text column carrying six
    // encodings, so nothing downstream could tell a benchmark case from a calibration probe without
    // parsing a string — and the alerting window keyed on that string, which made every compare cell
    // a unique key that never accumulated. The legacy label stays verbatim beside these.
    "ALTER TABLE scores ADD COLUMN rubric_id TEXT",
    "ALTER TABLE scores ADD COLUMN kind TEXT",
    "CREATE INDEX IF NOT EXISTS idx_scores_rubric_id ON scores(rubric_id, created_at)",
    "CREATE INDEX IF NOT EXISTS idx_scores_kind ON scores(kind, created_at)",
    // A rubric edit changes what a score *means*, and nothing recorded that one had happened.
    // A new version is a new row linked to the old one, never a mutation of it.
    "ALTER TABLE rubrics ADD COLUMN version INTEGER NOT NULL DEFAULT 1",
    "ALTER TABLE rubrics ADD COLUMN supersedes TEXT",
];

/// Columns added **after** the [`SCHEMA`] batch instead of before it.
///
/// The pre-batch list above exists for columns the batch's own indexes reference, which must be in
/// place before `CREATE INDEX` runs. These have the opposite requirement: nothing in [`SCHEMA`]
/// indexes them, and on a *fresh* database their table does not exist until the batch creates it —
/// so an `ALTER` attempted first is a tolerated "no such table" and the column never appears at all.
/// Running them here widens the table the batch just created, and is still a no-op ("duplicate
/// column name") on a database that already has them.
///
/// The alternative — editing the `CREATE TABLE` in `schema/sqlite/001_init.sql` — would put the
/// same fact in two places, which is how the two drift.
const ADDED_COLUMNS_LATE: &[&str] = &[
    // Measure-to-act guardrails (M4): a threshold that is not a bare number (`threshold_json`), a
    // forecast-driven action override and its deadline, what created the rule, and when a
    // policy-created rule lapses. All nullable — an existing row reads back as exactly the fixed,
    // human-made, never-expiring rule it always was.
    "ALTER TABLE limit_rules ADD COLUMN threshold_json TEXT",
    "ALTER TABLE limit_rules ADD COLUMN escalation_json TEXT",
    "ALTER TABLE limit_rules ADD COLUMN escalated_until TEXT",
    "ALTER TABLE limit_rules ADD COLUMN origin TEXT",
    "ALTER TABLE limit_rules ADD COLUMN expires_at TEXT",
    // M7 — the relay's fenced, renewable lease: `failures` is the retry budget, `stale_reclaims`
    // counts device deaths (kept apart so a sleeping laptop does not burn a chance), `lease_fence` is
    // the holding device's identity compared exactly on settle/renew/progress, `progress` its liveness.
    "ALTER TABLE relay_tasks ADD COLUMN failures INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE relay_tasks ADD COLUMN stale_reclaims INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE relay_tasks ADD COLUMN lease_fence TEXT",
    "ALTER TABLE relay_tasks ADD COLUMN progress TEXT",
    // M23 — the served-version quality loop on the prompt row: the canary policy (JSON, NULL = the
    // registry still stops observing a version at promotion) and the append-only label ledger
    // (JSON array, NULL = a prompt whose labels have not moved since the column existed). Both
    // nullable, so an existing registry row reads back as exactly the prompt it always was.
    "ALTER TABLE prompts ADD COLUMN canary TEXT",
    "ALTER TABLE prompts ADD COLUMN label_history TEXT",
    // The quality read joins scores to events by event_id and windows on the VERDICT's created_at;
    // without this the join degrades to a scan of the scores table per window.
    "CREATE INDEX IF NOT EXISTS idx_scores_created ON scores(created_at)",
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
    // …and the columns AGAIN, after the batch. On a *fresh* database the first pass skipped every
    // statement ("no such table" — nothing exists yet), so a column added here but not also written
    // into [`SCHEMA`]'s `CREATE TABLE` would be missing for the whole life of the process that
    // created the file, and appear only on the next open. That is a genuinely nasty shape: the bug
    // reproduces once, on a new install, and never again on the developer's machine. Running the
    // list on both sides makes [`ADDED_COLUMNS`] self-sufficient, and the second pass costs a
    // handful of "duplicate column name" errors on every open.
    // ...together with the columns that may only be added after the batch (see
    // [`ADDED_COLUMNS_LATE`]).
    for stmt in ADDED_COLUMNS.iter().chain(ADDED_COLUMNS_LATE) {
        add_column(c, stmt)?;
    }
    if backfill {
        c.execute(
            "UPDATE events SET received_at = ts WHERE received_at IS NULL",
            [],
        )?;
    }
    dated_price_book(c)?;
    Ok(())
}

/// M26 — turn `model_prices` from one overwritten row per model into a dated, append-only table
/// keyed `(provider, model, effective_from)`.
///
/// This is the one migration in the file that an `ALTER` cannot express: SQLite can add columns but
/// not change a primary key, so the table has to be rebuilt. Detection is by *shape*, not by a
/// version counter — if `effective_from` is already a column the rebuild is skipped — which makes it
/// idempotent on every open, including the very first one on a fresh database (where the `CREATE
/// TABLE IF NOT EXISTS` in `001_init.sql` has just laid down the pre-M26 shape and this immediately
/// replaces it, on an empty table).
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
