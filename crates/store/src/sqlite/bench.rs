//! In-process ingest-under-read-load harness. `#[ignore]`d: it asserts nothing, it *measures*, and
//! the numbers depend on the machine's disk. Run it by hand when changing the concurrency model:
//!
//! ```text
//! cargo test -p lighttrack-store --lib -- --ignored --nocapture ingest_under_read_load
//! ```
//!
//! It opens the same workload twice — once with the pre-change store shape (rollback journal, every
//! call through the single connection mutex) and once with WAL + the read pool — and reports writer
//! throughput and p95 write latency while N reader threads run dashboard-shaped queries.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use super::tests_concurrency::ev;
use super::{OpenOpts, SqliteStore};
use crate::Store;

const SEED_EVENTS: usize = 4_000;
const WRITES: usize = 400;
const READERS: usize = 4;

struct Measured {
    writes_per_sec: f64,
    p95_write_ms: f64,
    reads_per_sec: f64,
}

fn measure(label: &str, opts: OpenOpts) -> Measured {
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(SqliteStore::open_with(dir.path().join("lt.db"), opts).unwrap());

    for _ in 0..(SEED_EVENTS / 200) {
        let evs: Vec<_> = (0..200).map(|_| ev("bench")).collect();
        for r in s.insert_events_checked(&evs) {
            r.unwrap();
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..READERS)
        .map(|_| {
            let (s, stop) = (s.clone(), stop.clone());
            thread::spawn(move || {
                let mut n = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    s.cost_summary(Some("bench")).unwrap();
                    s.list_events(Some("bench"), 200).unwrap();
                    n += 2;
                }
                n
            })
        })
        .collect();

    // Let the readers get going so the writer really is competing with them.
    thread::sleep(Duration::from_millis(200));

    let mut lat = Vec::with_capacity(WRITES);
    let t0 = Instant::now();
    for _ in 0..WRITES {
        let e = ev("bench");
        let t = Instant::now();
        s.insert_event_checked(&e).unwrap();
        lat.push(t.elapsed());
    }
    let elapsed = t0.elapsed();
    stop.store(true, Ordering::Relaxed);
    let reads: usize = readers.into_iter().map(|h| h.join().unwrap()).sum();

    lat.sort();
    let p95 = lat[(lat.len() * 95 / 100).min(lat.len() - 1)].as_secs_f64() * 1000.0;
    // Reads are normalised per second: the writer's own run time is the measurement window, and a
    // faster writer shortens it — raw read counts would read as a regression when nothing regressed.
    let m = Measured {
        writes_per_sec: WRITES as f64 / elapsed.as_secs_f64(),
        p95_write_ms: p95,
        reads_per_sec: reads as f64 / elapsed.as_secs_f64(),
    };
    println!(
        "{label:<32} ingest {:>8.1}/s   p95 write {:>7.2} ms   reads {:>8.1}/s",
        m.writes_per_sec, m.p95_write_ms, m.reads_per_sec
    );
    m
}

#[test]
#[ignore]
fn ingest_under_read_load() {
    println!("\n{READERS} reader threads, {SEED_EVENTS} seeded events, {WRITES} measured writes\n");
    // The middle row is the honest "before": WAL was already engaged in practice, because the schema
    // batch opens with `PRAGMA journal_mode = WAL` — it was simply never read back or relied upon.
    measure("rollback journal, no pool", OpenOpts { wal: false, read_pool: 0 });
    let before = measure("before: WAL, single mutex", OpenOpts { wal: true, read_pool: 0 });
    let after = measure("after: WAL + read pool", OpenOpts { wal: true, read_pool: 4 });
    println!(
        "\nvs. before — ingest x{:.2}   p95 x{:.2} (lower is better)   reads x{:.2}\n",
        after.writes_per_sec / before.writes_per_sec,
        after.p95_write_ms / before.p95_write_ms,
        after.reads_per_sec / before.reads_per_sec,
    );
}
