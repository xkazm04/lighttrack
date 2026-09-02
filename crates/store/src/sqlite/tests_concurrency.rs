//! Concurrency + on-disk journal tests for the read pool.
//!
//! These are the tests that must fail if the pool ever stops being (a) genuinely read-only and
//! (b) snapshot-isolated from an in-flight write. Everything here needs a *file* database — an
//! in-memory one is private to its connection and has no WAL, so it can't exercise the pool.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use rusqlite::Connection;

use lighttrack_core::{new_id, LlmEvent, Operation, Status, TokenUsage};

use super::{events, schema, SqliteStore};
use crate::Store;

pub(super) fn ev(project: &str) -> LlmEvent {
    LlmEvent {
        id: new_id(),
        project_id: project.into(),
        trace_id: None,
        span_id: None,
        parent_span_id: None,
        ts: Utc::now(),
        received_at: Utc::now(),
        provider: "anthropic".into(),
        model: "claude-haiku-4-5".into(),
        name: None,
        operation: Operation::Chat,
        usage: TokenUsage {
            input: 10,
            output: 5,
            cached_input: None,
            reasoning: None,
        },
        cost_usd: Some(0.001),
        latency_ms: Some(12),
        status: Status::Success,
        error: None,
        input: None,
        output: None,
        tags: vec![],
        source: None,
        metadata: serde_json::json!({}),
    }
}

fn journal_mode(c: &Connection) -> String {
    c.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
        .unwrap()
        .to_lowercase()
}

#[test]
fn wal_is_engaged_on_disk_and_seen_by_pooled_readers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lt.db");
    let s = SqliteStore::open(&path).unwrap();
    s.insert_event(&ev("p1")).unwrap();

    // Asserted, not merely requested: read the pragma back on the write connection…
    assert_eq!(s.with(journal_mode), "wal");
    // …and on a pooled reader, which is a different connection to the same file.
    let reader = s
        .readers
        .acquire()
        .expect("read pool should be enabled for a file database");
    assert_eq!(journal_mode(&reader), "wal");
    drop(reader);
    assert!(s.readers.size() > 0);

    // The sidecars WAL implies really are on disk (packaging/backup must copy them, or checkpoint).
    let wal = path.with_file_name("lt.db-wal");
    assert!(
        wal.exists(),
        "expected the -wal sidecar at {}",
        wal.display()
    );
}

#[test]
fn pooled_readers_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let s = SqliteStore::open(dir.path().join("lt.db")).unwrap();
    let reader = s.readers.acquire().expect("read pool");

    // SQLITE_OPEN_READ_ONLY is the structural guarantee that a "read" can never interleave with the
    // write connection's admission transaction: it cannot start a write at all.
    let err = reader.execute("DELETE FROM events", []).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("readonly")
            || err.to_string().to_lowercase().contains("read-only"),
        "pooled connection accepted a write, or failed for the wrong reason: {err}"
    );
}

#[test]
fn reads_see_a_consistent_snapshot_while_a_write_transaction_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(SqliteStore::open(dir.path().join("lt.db")).unwrap());
    s.insert_event(&ev("p1")).unwrap();

    const HELD: Duration = Duration::from_millis(400);
    let gate = Arc::new(Barrier::new(2));
    let writer = {
        let (s, gate) = (s.clone(), gate.clone());
        thread::spawn(move || {
            // Hold the write connection across an open transaction — exactly the shape of a batch
            // admission (`insert_events_checked`), which writes N rows before committing once.
            s.with(|c| {
                let tx = c.unchecked_transaction().unwrap();
                for _ in 0..5 {
                    events::insert(&tx, &ev("p1")).unwrap();
                }
                gate.wait();
                thread::sleep(HELD);
                tx.commit().unwrap();
            });
        })
    };

    gate.wait();
    let t0 = Instant::now();
    let seen = s.list_events(None, 1_000).unwrap().len();
    let waited = t0.elapsed();

    // Isolation: the five uncommitted rows are invisible — no half-applied batch can be read.
    assert_eq!(
        seen, 1,
        "a read observed rows from an uncommitted write transaction"
    );
    // Concurrency: this is the assertion that fails on the old single-mutex store, where the read
    // could not even start until the writer let go.
    assert!(
        waited < HELD / 2,
        "read blocked behind the open write transaction for {waited:?}"
    );

    writer.join().unwrap();
    assert_eq!(s.list_events(None, 1_000).unwrap().len(), 6);
}

#[test]
fn readers_never_observe_a_half_applied_batch_admission() {
    const BATCH: usize = 40;
    const ROUNDS: usize = 20;

    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(SqliteStore::open(dir.path().join("lt.db")).unwrap());
    let stop = Arc::new(AtomicBool::new(false));

    // Live poll count, so the writer can WAIT for the reader to actually be reading before it starts.
    // `polls > 0` used to be checked only after the join, which made it an assertion about the
    // SCHEDULER rather than about the invariant: on a loaded machine the spawned thread could still
    // be unscheduled when the writer finished its 800 inserts, and the test failed saying "the reader
    // never ran" while the store had done nothing wrong. Observed 2026-08-24 once in twelve runs of
    // the lib suite, after this wave added ~20 disk-heavy tests to the same binary. Waiting for the
    // first poll makes the window real instead of hoped-for — and the wait has its own deadline, so
    // a reader that genuinely cannot run still fails loudly rather than being papered over.
    let polls_seen = Arc::new(AtomicUsize::new(0));

    let reader = {
        let (s, stop, seen) = (s.clone(), stop.clone(), polls_seen.clone());
        thread::spawn(move || {
            let mut polls = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let n = s.list_events(None, 100_000).unwrap().len();
                assert_eq!(
                    n % BATCH,
                    0,
                    "read saw {n} events — a batch was partially visible"
                );
                polls += 1;
                seen.store(polls, Ordering::Relaxed);
            }
            polls
        })
    };

    let waited_from = Instant::now();
    while polls_seen.load(Ordering::Relaxed) == 0 {
        assert!(
            waited_from.elapsed() < Duration::from_secs(10),
            "the reader never got a poll in 10s — the test would prove nothing, so it fails here              rather than after the fact"
        );
        thread::yield_now();
    }

    for _ in 0..ROUNDS {
        let evs: Vec<LlmEvent> = (0..BATCH).map(|_| ev("p1")).collect();
        for r in s.insert_events_checked(&evs) {
            r.unwrap();
        }
    }
    stop.store(true, Ordering::Relaxed);
    let polls = reader.join().unwrap();

    assert!(polls > 0, "the reader never ran — the test proved nothing");
    assert_eq!(s.list_events(None, 100_000).unwrap().len(), BATCH * ROUNDS);
}

#[test]
fn a_pre_wal_database_upgrades_cleanly_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");

    // Build a database the way an older LightTrack left it: rollback journal, and without the
    // additively-migrated `received_at` column.
    {
        let c = Connection::open(&path).unwrap();
        schema::apply(&c).unwrap();
        c.execute_batch(
            "DROP INDEX IF EXISTS idx_events_project_received;
             ALTER TABLE events DROP COLUMN received_at;",
        )
        .unwrap();
        c.execute(
            "INSERT INTO events (id, project_id, ts, provider, model, operation, input_tokens, \
             output_tokens, status, tags, metadata) VALUES ('legacy-1','p1', \
             '2026-01-01T00:00:00.000000000Z','anthropic','claude-haiku-4-5','chat',10,5,\
             'success','[]','{}')",
            [],
        )
        .unwrap();
        let mode: String = c
            .query_row("PRAGMA journal_mode=DELETE", [], |r| r.get::<_, String>(0))
            .unwrap()
            .to_lowercase();
        assert_eq!(mode, "delete", "fixture must start out pre-WAL");
    }

    let s = SqliteStore::open(&path).unwrap();
    assert_eq!(
        s.with(journal_mode),
        "wal",
        "opening an existing database must upgrade it to WAL"
    );
    assert!(
        s.readers.size() > 0,
        "the pool must come up on a migrated legacy database"
    );

    // The legacy row survived and was backfilled (arrival time = event time), and it is readable
    // through the pool.
    let got = s.get_event("legacy-1").unwrap().expect("legacy row");
    assert_eq!(got.received_at, got.ts);
    assert_eq!(got.project_id, "p1");

    // And the upgraded database still takes writes.
    s.insert_event(&ev("p1")).unwrap();
    assert_eq!(s.list_events(None, 10).unwrap().len(), 2);
}

#[test]
fn a_disabled_pool_still_serves_reads_through_the_write_connection() {
    let dir = tempfile::tempdir().unwrap();
    let s = SqliteStore::open_with(
        dir.path().join("lt.db"),
        super::OpenOpts {
            wal: false,
            read_pool: 0,
        },
    )
    .unwrap();
    assert_eq!(s.readers.size(), 0);
    assert_ne!(s.with(journal_mode), "wal");

    s.insert_event(&ev("p1")).unwrap();
    assert_eq!(s.list_events(None, 10).unwrap().len(), 1);
}
