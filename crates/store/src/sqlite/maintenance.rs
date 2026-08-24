//! Disk accounting and **lossless** maintenance for the embedded store.
//!
//! An embedded store grows monotonically by default: every event, score and job row is appended by
//! code that will never think about it again, into a file in a directory the application does not
//! monitor. This module is the half of that problem engineering can close without making a product
//! decision — *measure it, and reclaim what is already dead* — and it deliberately contains no
//! pruner.
//!
//! **Retention is deliberately unbounded (operator decision, 2026-08-24.)** Nothing here deletes a
//! row, and no age-floor sweep over `events`, `scores` or `jobs` exists, because keeping the data is
//! the current policy — see [`RETENTION_NOTE`], which is carried *inside the report payload* so an
//! operator reading their disk usage reads the retention stance in the same breath. The two acts
//! this module does perform are lossless by construction:
//!
//! * **Checkpoint** — move already-committed pages out of the write-ahead journal into the database
//!   file. Nothing is lost; the sidecar shrinks.
//! * **Incremental vacuum** — hand pages the engine has *already freed* back to the filesystem.
//!   Nothing is lost; the file shrinks.
//!
//! Deleting rows does not shrink the file (the engine recycles freed pages internally), so
//! "reclaimable" here means exactly the freelist: space the store already owns and is not using.
//! That number, not a schedule, is what triggers reclamation.
//!
//! ## Why incremental, and what an old file cannot do
//!
//! A full `VACUUM` is a whole-file rewrite: it cannot be chunked, it holds the store for its whole
//! duration, and it transiently needs up to twice the file size in free disk — the tool that frees
//! space must not be the tool that fills the disk. So the routine path is
//! `PRAGMA incremental_vacuum(N)`, which returns exactly N pages and can be stopped at any chunk
//! boundary, leaving the store consistent.
//!
//! Incremental vacuum requires `auto_vacuum=INCREMENTAL`, a property of the *file* fixed when it is
//! created. New databases get it ([`super::schema::apply`] sets it before the first table exists).
//! A database created before 2026-08-24 has `auto_vacuum=none` and cannot reclaim incrementally at
//! all — the report says so, in that file's own row, with the offline remedy and the disk it needs,
//! rather than silently reporting zero pages reclaimed forever.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use rusqlite::Connection;

use crate::{
    ByteMeasure, MaintenanceOutcome, MaintenancePass, MaintenanceRequest, Result, StorageObject,
    StorageReport,
};

/// The current retention stance, carried in every storage report.
///
/// Dated and attributed on purpose: an unbounded-growth policy that nobody wrote down is
/// indistinguishable from an unbounded-growth policy nobody noticed, and the difference is the whole
/// finding. When retention is revisited, this string and `docs/OPERATIONS.md` change together.
pub(super) const RETENTION_NOTE: &str =
    "retention deliberately unbounded (operator 2026-08-24): no \
     row in events, scores, jobs or any other table is ever deleted by this process. The single \
     exception is collective_entries, which a hub prunes past an age floor because it holds other \
     instances' contributions rather than this instance's own history. Disk therefore grows \
     monotonically with ingest; this report is how that growth stays visible, and maintenance \
     reclaims only space the engine has already freed.";

/// Pages of already-freed space one maintenance chunk returns to the filesystem. Small on purpose:
/// the chunk boundary is where the caller re-reads its activity gauge, so a chunk must be short
/// enough that a user arriving mid-pass waits for one chunk, not for the whole reclamation.
pub(super) const DEFAULT_RECLAIM_CHUNK_PAGES: u32 = 256;

/// `PRAGMA auto_vacuum` as a name. The integer codes are 0/1/2 and mean nothing to a reader.
pub(super) fn auto_vacuum_mode(c: &Connection) -> Result<&'static str> {
    let v: i64 = c.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
    Ok(match v {
        1 => "full",
        2 => "incremental",
        _ => "none",
    })
}

fn scalar_u64(c: &Connection, pragma: &str) -> Result<u64> {
    let v: i64 = c.query_row(pragma, [], |r| r.get(0))?;
    Ok(v.max(0) as u64)
}

/// Per-object bytes from the engine's own page accounting.
///
/// Returns `None` — not an empty map — when `dbstat` is not compiled into this SQLite, so the caller
/// can report "not measured" rather than "measured as zero". (The bundled build this crate ships
/// does compile it in; the branch exists because a system-SQLite build might not, and a report that
/// answers 0 bytes on such a build is worse than one that admits it cannot see.)
fn object_bytes(c: &Connection) -> Option<BTreeMap<String, u64>> {
    let mut stmt = c
        .prepare("SELECT name, SUM(pgsize) FROM dbstat GROUP BY name")
        .ok()?;
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1).unwrap_or(0)))
        })
        .ok()?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (name, bytes) = row.ok()?;
        out.insert(name, bytes.max(0) as u64);
    }
    Some(out)
}

/// Every table and index in the schema, with each table's row count.
///
/// A count per table is 17 index scans on this schema — cheap enough for an on-demand operator
/// surface, and the number that separates "the query got slower" from "the table got bigger".
fn objects(
    c: &Connection,
    bytes: Option<&BTreeMap<String, u64>>,
    total: u64,
) -> Vec<StorageObject> {
    let mut out = Vec::new();
    let mut stmt = match c.prepare(
        "SELECT name, type FROM sqlite_master \
         WHERE type IN ('table','index') AND name NOT LIKE 'sqlite_%' ORDER BY name",
    ) {
        Ok(s) => s,
        Err(_) => return out,
    };
    let listed: Vec<(String, String)> = match stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))) {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => return out,
    };
    for (name, kind) in listed {
        // Only tables have a row count; an index's rows are its table's, and reporting them twice
        // would double the denominator of every "share of rows" anyone computes from this.
        let rows = if kind == "table" {
            // The name comes from sqlite_master, not from a caller, so it cannot be an injection —
            // but it still cannot be bound (an identifier is not a value), so it is quoted through
            // SQLite's own `"` escaping rather than interpolated raw.
            let sql = format!("SELECT COUNT(*) FROM \"{}\"", name.replace('"', "\"\""));
            c.query_row(&sql, [], |r| r.get::<_, i64>(0)).ok()
        } else {
            None
        };
        let b = bytes.and_then(|m| m.get(&name).copied());
        out.push(StorageObject {
            name,
            kind,
            rows,
            bytes: b,
            share: b.and_then(|b| (total > 0).then_some(b as f64 / total as f64)),
        });
    }
    // Largest first: the report exists to name the object that is big, and a reader should not have
    // to sort seventeen rows by eye to find it.
    out.sort_by(|a, b| {
        b.bytes
            .unwrap_or(0)
            .cmp(&a.bytes.unwrap_or(0))
            .then_with(|| b.rows.unwrap_or(0).cmp(&a.rows.unwrap_or(0)))
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// Build the accounting report from a read-only connection plus the file on disk.
pub(super) fn report(c: &Connection, path: Option<&Path>) -> Result<StorageReport> {
    let page_size = scalar_u64(c, "PRAGMA page_size")?;
    let page_count = scalar_u64(c, "PRAGMA page_count")?;
    let freelist = scalar_u64(c, "PRAGMA freelist_count")?;
    let db_bytes = page_count * page_size;
    let reclaimable_bytes = freelist * page_size;
    let auto_vacuum = auto_vacuum_mode(c)?;

    // The sidecar is a real file and a real part of "how much disk is this costing me"; the engine's
    // page accounting does not see it. A failed stat stays `None` — a WAL that cannot be measured is
    // not a WAL of zero bytes.
    let wal_bytes = path.and_then(|p| {
        let wal = p.with_file_name(format!("{}-wal", p.file_name()?.to_string_lossy()));
        std::fs::metadata(wal).ok().map(|m| m.len())
    });

    let map = object_bytes(c);
    let measured = if map.is_some() {
        ByteMeasure::PagesAllocated
    } else {
        ByteMeasure::Unavailable
    };
    let objects = objects(c, map.as_ref(), db_bytes);

    let reclaim_note = if auto_vacuum == "incremental" {
        format!(
            "incremental: maintenance returns free pages to the filesystem {DEFAULT_RECLAIM_CHUNK_PAGES} at a time, \
             yielding between chunks"
        )
    } else {
        format!(
            "unavailable on this file: auto_vacuum={auto_vacuum} is fixed at creation time and this \
             database predates the incremental setting (2026-08-24), so free pages are reused but \
             never returned to the filesystem. {reclaimable_bytes} bytes are currently reclaimable. \
             The remedy is offline and manual — stop the API and run `VACUUM;` once (a full rewrite: \
             it needs roughly {} bytes of free disk on this volume, and afterwards \
             `PRAGMA auto_vacuum=INCREMENTAL; VACUUM;` makes future reclamation incremental).",
            db_bytes.saturating_mul(2)
        )
    };

    Ok(StorageReport {
        backend: "sqlite",
        path: path.map(|p| p.display().to_string()),
        page_size,
        db_bytes,
        wal_bytes,
        reclaimable_bytes,
        reclaimable_share: if db_bytes > 0 {
            reclaimable_bytes as f64 / db_bytes as f64
        } else {
            0.0
        },
        auto_vacuum,
        reclaim_note,
        measured,
        bytes_predicate: measured.predicate(),
        objects,
        retention: RETENTION_NOTE,
    })
}

/// Run one lossless maintenance chunk on the **write** connection.
///
/// Order matters: checkpoint first, then reclaim. A checkpoint can *create* free pages (it is what
/// moves committed deletions from the journal into the file), so reclaiming first would leave this
/// pass's own freed space for the next one.
pub(super) fn pass(c: &Connection, req: MaintenanceRequest) -> Result<MaintenancePass> {
    let t0 = Instant::now();
    let freelist_before = scalar_u64(c, "PRAGMA freelist_count")?;
    let mut detail = String::new();
    let mut failed = false;

    // `PRAGMA wal_checkpoint(...)` answers three columns: busy, pages in the journal, pages moved.
    //
    // PASSIVE always runs first, and it is the measurement: it never blocks (or is blocked by) a
    // reader or a writer, and it is the only mode that REPORTS what it did. TRUNCATE answers
    // `(0, 0, 0)` on success — the journal it would have counted no longer exists by the time it
    // returns — so asking only TRUNCATE would make every successful pass indistinguishable from a
    // pass that found nothing to do, which is exactly the confusion the three outcomes exist to
    // prevent. TRUNCATE then runs as the *second* step when asked: the escalation rung that cuts the
    // sidecar back to zero bytes, reached only when the sidecar's own size is the stated harm.
    let checkpointed = match c.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| {
        Ok((
            r.get::<_, i64>(0).unwrap_or(0),
            r.get::<_, i64>(1).unwrap_or(0),
            r.get::<_, i64>(2).unwrap_or(0),
        ))
    }) {
        Ok((busy, in_wal, moved)) => {
            if busy != 0 || (in_wal > 0 && moved < in_wal) {
                detail.push_str(&format!(
                    "passive checkpoint was held up by a live reader/writer (busy={busy}); \
                     {moved} of {in_wal} journal pages moved. "
                ));
            }
            moved.max(0) as u64
        }
        Err(e) => {
            // Not a failure worth aborting the pass for on a store with no journal at all (an
            // in-memory database), but it is not a success either: say which it was.
            failed = true;
            detail.push_str(&format!("passive checkpoint failed: {e}. "));
            0
        }
    };
    if req.truncate_wal && !failed {
        if let Err(e) = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())) {
            failed = true;
            detail.push_str(&format!("truncating checkpoint failed: {e}. "));
        }
    }

    let mut reclaimed = 0u64;
    if req.reclaim_pages > 0 && freelist_before > 0 {
        match auto_vacuum_mode(c)? {
            "incremental" => {
                // `PRAGMA incremental_vacuum(N)` yields ONE EMPTY ROW PER PAGE FREED, so it has to
                // be stepped to completion — `execute_batch` steps once and stops, which reclaims a
                // single page and looks like success. (Found the honest way: the chunk loop in
                // `tests_maintenance` never drained the freelist.)
                let sql = format!("PRAGMA incremental_vacuum({})", req.reclaim_pages);
                let stepped = (|| -> Result<()> {
                    let mut stmt = c.prepare(&sql)?;
                    let mut rows = stmt.query([])?;
                    while rows.next()?.is_some() {}
                    Ok(())
                })();
                if let Err(e) = stepped {
                    failed = true;
                    detail.push_str(&format!("incremental_vacuum failed: {e}. "));
                }
            }
            mode => {
                detail.push_str(&format!(
                    "no reclamation attempted: auto_vacuum={mode} on this file, so free pages \
                     cannot be returned without a full offline VACUUM. "
                ));
            }
        }
    }
    let freelist_after = scalar_u64(c, "PRAGMA freelist_count")?;
    reclaimed += freelist_before.saturating_sub(freelist_after);

    let outcome = if failed {
        MaintenanceOutcome::Failed
    } else if checkpointed > 0 || reclaimed > 0 {
        MaintenanceOutcome::Ran
    } else {
        MaintenanceOutcome::NothingToDo
    };
    if detail.is_empty() {
        detail.push_str(match outcome {
            MaintenanceOutcome::Ran => "checkpointed and/or reclaimed",
            MaintenanceOutcome::NothingToDo => {
                "journal was already checkpointed and no free pages were pending"
            }
            MaintenanceOutcome::Failed => "failed",
        });
    }

    Ok(MaintenancePass {
        outcome,
        duration_ms: t0.elapsed().as_millis() as u64,
        pages_checkpointed: checkpointed,
        pages_reclaimed: reclaimed,
        freelist_before,
        freelist_after,
        detail: detail.trim_end().to_string(),
    })
}
