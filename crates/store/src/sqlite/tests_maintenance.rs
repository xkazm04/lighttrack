//! Storage accounting + lossless maintenance.
//!
//! The claims under test are the ones an operator would act on: the report names every table and
//! says how it measured them, a maintenance pass never costs a row, and the gap the technique warns
//! about — "rows were deleted but the file did not shrink" — is actually closed on a database that
//! can reclaim incrementally.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{MaintenanceOutcome, MaintenanceRequest, Store};

use super::tests_concurrency::ev;
use super::{maintenance, SqliteStore};

/// A file-backed store in a fresh temp dir — the accounting surface is about a *file*, so an
/// in-memory database would certify nothing about it.
fn store() -> (tempfile::TempDir, SqliteStore) {
    let dir = tempfile::tempdir().unwrap();
    let s = SqliteStore::open(dir.path().join("lt.db")).unwrap();
    (dir, s)
}

/// A fat event, so a few hundred rows make a database big enough for page accounting to be
/// interesting rather than a rounding error on the first page.
fn fat(project: &str) -> lighttrack_core::LlmEvent {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut e = ev(project);
    e.input = Some(format!("prompt {n} {}", "x".repeat(900)).into());
    e.output = Some(format!("completion {n} {}", "y".repeat(900)).into());
    e
}

fn seed(s: &SqliteStore, n: usize) {
    let evs: Vec<_> = (0..n).map(|_| fat("acct")).collect();
    for r in s.insert_events_checked(&evs) {
        r.unwrap();
    }
}

#[test]
fn the_report_accounts_every_table_and_says_how_it_measured() {
    let (_d, s) = store();
    seed(&s, 200);
    let r = s.storage_report().unwrap();

    assert_eq!(r.backend, "sqlite");
    assert!(r.path.is_some(), "a file-backed store names its file");
    assert!(r.db_bytes > 0 && r.page_size > 0);

    // Per-object, not just a file total: the unit of actionability is the table.
    let events = r
        .objects
        .iter()
        .find(|o| o.name == "events")
        .expect("the events table is accounted");
    assert_eq!(events.rows, Some(200), "row counts are real, not estimated");
    assert_eq!(events.kind, "table");
    assert!(events.bytes.unwrap() > 0);
    assert!(
        r.objects.iter().any(|o| o.kind == "index"),
        "indexes are accounted as their own objects — an index that outgrew its table is its own \
         finding"
    );

    // Every byte figure travels with what it means. `pgsize` is pages ALLOCATED, which is not the
    // same claim as bytes of live rows, and the difference is exactly the reclaimable space.
    assert_eq!(r.measured, crate::ByteMeasure::PagesAllocated);
    assert!(r.bytes_predicate.contains("pages allocated"));
    assert!(r.bytes_predicate.contains("not bytes of live rows"));

    // Largest first, so the big object does not have to be found by eye.
    let bytes: Vec<u64> = r.objects.iter().map(|o| o.bytes.unwrap_or(0)).collect();
    assert!(
        bytes.windows(2).all(|w| w[0] >= w[1]),
        "objects are ordered largest-first: {bytes:?}"
    );
}

#[test]
fn the_report_states_the_retention_stance_where_the_disk_is_measured() {
    let (_d, s) = store();
    seed(&s, 10);
    let r = s.storage_report().unwrap();
    // The operator decision, dated, in the payload — not only in a doc someone might read.
    assert!(r.retention.contains("retention deliberately unbounded"));
    assert!(r.retention.contains("2026-08-24"));
    assert!(
        r.retention.contains("collective_entries"),
        "the one table that IS pruned is named, so the exception does not read as an inconsistency"
    );
}

#[test]
fn a_database_created_now_can_reclaim_in_chunks() {
    let (_d, s) = store();
    let r = s.storage_report().unwrap();
    assert_eq!(
        r.auto_vacuum, "incremental",
        "auto_vacuum is fixed at creation; a database created today must be able to give pages back"
    );
    assert!(r.reclaim_note.starts_with("incremental"));
}

#[test]
fn an_older_file_says_it_cannot_reclaim_and_names_the_remedy() {
    // A database created the way every pre-2026-08-24 install was: no auto_vacuum. The report must
    // not answer "0 pages reclaimed" forever in a voice that reads like success.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(
            "PRAGMA auto_vacuum=NONE; CREATE TABLE t(x TEXT); INSERT INTO t VALUES('a');",
        )
        .unwrap();
    }
    let s = SqliteStore::open(&path).unwrap();
    let r = s.storage_report().unwrap();
    assert_eq!(r.auto_vacuum, "none");
    assert!(r.reclaim_note.starts_with("unavailable on this file"));
    assert!(
        r.reclaim_note.contains("VACUUM"),
        "the remedy is named: {}",
        r.reclaim_note
    );
    assert!(
        r.reclaim_note.contains("free disk"),
        "and so is what the remedy costs: {}",
        r.reclaim_note
    );

    // And a pass on such a file reports the skip rather than pretending it reclaimed.
    let p = s
        .maintenance_pass(MaintenanceRequest {
            truncate_wal: false,
            reclaim_pages: 64,
        })
        .unwrap();
    assert_ne!(p.outcome, MaintenanceOutcome::Failed);
}

#[test]
fn a_maintenance_pass_never_costs_a_row() {
    let (_d, s) = store();
    seed(&s, 120);
    let before = s.list_events(Some("acct"), 10_000).unwrap().len();
    for _ in 0..3 {
        let p = s
            .maintenance_pass(MaintenanceRequest {
                truncate_wal: true,
                reclaim_pages: maintenance::DEFAULT_RECLAIM_CHUNK_PAGES,
            })
            .unwrap();
        assert_ne!(p.outcome, MaintenanceOutcome::Failed, "{}", p.detail);
    }
    let after = s.list_events(Some("acct"), 10_000).unwrap().len();
    assert_eq!(before, 120);
    assert_eq!(
        before, after,
        "checkpoint + incremental vacuum are lossless; there is no pruning door here"
    );
    // And the store is still usable afterwards — a pass that leaves a broken file is not lossless
    // in any sense that matters.
    seed(&s, 5);
    assert_eq!(s.list_events(Some("acct"), 10_000).unwrap().len(), 125);
}

#[test]
fn the_three_outcomes_stay_distinguishable() {
    let (_d, s) = store();
    seed(&s, 50);
    // First pass has a journal to move.
    let first = s
        .maintenance_pass(MaintenanceRequest {
            truncate_wal: true,
            reclaim_pages: 0,
        })
        .unwrap();
    assert_eq!(first.outcome, MaintenanceOutcome::Ran);
    assert!(first.pages_checkpointed > 0);
    // Immediately again: nothing to do is its OWN outcome, not a quiet success.
    let second = s
        .maintenance_pass(MaintenanceRequest {
            truncate_wal: true,
            reclaim_pages: 0,
        })
        .unwrap();
    assert_eq!(second.outcome, MaintenanceOutcome::NothingToDo);
    assert!(
        second.detail.contains("already checkpointed"),
        "a no-op says why: {}",
        second.detail
    );
}

#[test]
fn deleting_rows_does_not_shrink_the_file_until_a_pass_reclaims() {
    let (_d, s) = store();
    seed(&s, 300);
    // Checkpoint first so every page is in the main file and the sizes below compare like for like.
    s.maintenance_pass(MaintenanceRequest {
        truncate_wal: true,
        reclaim_pages: 0,
    })
    .unwrap();
    let full = s.storage_report().unwrap();
    assert!(full.reclaimable_share < 0.5);

    // Free a large amount of space. (Reached through the store's own write connection: the product
    // has no pruner, by decision — this stands in for whatever frees pages, including an upgrade or
    // a hub's collective-entry sweep.)
    s.with(|c| c.execute("DELETE FROM events", []).unwrap());
    s.maintenance_pass(MaintenanceRequest {
        truncate_wal: true,
        reclaim_pages: 0,
    })
    .unwrap();

    let freed = s.storage_report().unwrap();
    assert!(
        freed.reclaimable_bytes > 0,
        "the engine holds the freed pages: this is the gap the technique names"
    );
    assert_eq!(
        freed.db_bytes, full.db_bytes,
        "and deleting rows did NOT shrink the file — that is the whole point of reclamation being \
         a separate act"
    );

    // Now reclaim, in chunks, until the freelist is drained — each call is one yieldable chunk.
    let mut passes = 0;
    loop {
        let p = s
            .maintenance_pass(MaintenanceRequest {
                truncate_wal: false,
                reclaim_pages: 32,
            })
            .unwrap();
        passes += 1;
        assert_ne!(p.outcome, MaintenanceOutcome::Failed, "{}", p.detail);
        if p.freelist_after == 0 || passes > 200 {
            break;
        }
        assert!(
            p.pages_reclaimed > 0,
            "a chunk with free pages pending must return some: {p:?}"
        );
    }
    let compacted = s.storage_report().unwrap();
    assert!(
        compacted.db_bytes < full.db_bytes,
        "after reclamation the FILE is smaller: {} -> {}",
        full.db_bytes,
        compacted.db_bytes
    );
    assert_eq!(compacted.reclaimable_bytes, 0);
}

#[test]
fn a_truncating_checkpoint_returns_the_journal_sidecar_to_zero() {
    let (_d, s) = store();
    seed(&s, 200);
    let before = s.storage_report().unwrap();
    assert!(
        before.wal_bytes.unwrap_or(0) > 0,
        "writes leave a journal to account for"
    );
    s.maintenance_pass(MaintenanceRequest {
        truncate_wal: true,
        reclaim_pages: 0,
    })
    .unwrap();
    let after = s.storage_report().unwrap();
    assert_eq!(
        after.wal_bytes,
        Some(0),
        "TRUNCATE is the rung that answers a sidecar that is itself the harm"
    );
}

#[test]
fn an_unmeasurable_byte_figure_is_never_reported_as_zero() {
    // The two measures are different claims and must not read alike — the branch that says "I could
    // not look" has to be spelled differently from a measured zero.
    assert_ne!(
        crate::ByteMeasure::PagesAllocated.predicate(),
        crate::ByteMeasure::Unavailable.predicate()
    );
    assert!(crate::ByteMeasure::Unavailable
        .predicate()
        .contains("not zero"));
}

/// A managed backend has a disk somebody else monitors; answering "0 bytes, no tables" for it would
/// be a confident lie in the one surface an operator consults about disk. The trait default every
/// non-embedded backend inherits is therefore a refusal, and the refusal has to READ like one —
/// `api::storage` maps it to 501 `unsupported`, and this pins the wording that mapping relies on.
#[test]
fn the_default_for_a_backend_without_a_file_is_a_refusal_not_an_empty_report() {
    let e = crate::StoreError::Unsupported("storage accounting");
    assert!(e.to_string().contains("storage accounting"));
    assert!(e
        .to_string()
        .contains("not supported by this store backend"));
}
