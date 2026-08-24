//! The store measuring itself, end to end through the real `Store` methods.
//!
//! `metrics.rs` unit-tests the rate-limiter policy and the vocabulary; this pins the wiring — that
//! calling a method actually lands on the family an operator would look under, that waiting for a
//! connection is never folded into query time, and that the instrument does not use the database.

use std::collections::BTreeSet;

use crate::{DbOpStats, MaintenanceRequest, Store};

use super::tests_concurrency::ev;
use super::SqliteStore;

fn store() -> (tempfile::TempDir, SqliteStore) {
    let dir = tempfile::tempdir().unwrap();
    let s = SqliteStore::open(dir.path().join("lt.db")).unwrap();
    (dir, s)
}

fn stat<'a>(ops: &'a [DbOpStats], key: &str) -> Option<&'a DbOpStats> {
    ops.iter().find(|o| o.key == key)
}

#[test]
fn every_method_lands_on_the_family_an_operator_would_look_under() {
    let (_d, s) = store();
    for _ in 0..5 {
        s.insert_event(&ev("m")).unwrap();
    }
    s.list_events(Some("m"), 10).unwrap();
    s.list_events(Some("m"), 10).unwrap();
    s.cost_summary(Some("m")).unwrap();
    s.list_jobs(None, 10).unwrap();
    s.list_prices().unwrap();

    let r = s.db_metrics().unwrap();
    let ops = &r.ops;
    assert_eq!(stat(ops, "events.write").unwrap().count, 5);
    assert_eq!(stat(ops, "events.read").unwrap().count, 2);
    assert_eq!(
        stat(ops, "usage.read").unwrap().count,
        1,
        "a rolling cost aggregate is not an events point read — they scan differently and are \
         remedied differently"
    );
    assert_eq!(stat(ops, "jobs.read").unwrap().count, 1);
    // Anything not explicitly keyed still lands somewhere: an unkeyed family would be a blind spot
    // that reads as silence.
    assert_eq!(stat(ops, "other.read").unwrap().count, 1, "list_prices");

    // Keys are families, never statements.
    for o in ops {
        assert!(
            !o.key.contains(' ') && !o.key.contains('?'),
            "key looks like statement text: {}",
            o.key
        );
    }
}

#[test]
fn waiting_for_a_connection_is_its_own_key_and_never_inside_query_time() {
    let (_d, s) = store();
    s.insert_event(&ev("m")).unwrap();
    s.list_events(Some("m"), 10).unwrap();
    let r = s.db_metrics().unwrap();

    let acquire = stat(&r.ops, "pool.acquire").expect("pooled reads record their acquisition");
    let lock = stat(&r.ops, "write.lock.wait").expect("writers record their wait for the mutex");
    assert!(acquire.count >= 1 && lock.count >= 1);
    // Disjoint remedies, disjoint keys, disjoint thresholds.
    assert_ne!(
        acquire.slow_over_ms,
        stat(&r.ops, "events.read").unwrap().slow_over_ms
    );
}

#[test]
fn a_slow_count_never_travels_without_its_predicate() {
    let (_d, s) = store();
    s.insert_event(&ev("m")).unwrap();
    let r = s.db_metrics().unwrap();
    for o in &r.ops {
        assert!(
            o.slow_over_ms > 0.0,
            "{} reports a slow count with no threshold — 'N slow queries' is a number two people \
             read differently",
            o.key
        );
    }
    // And the report says how every figure was recomputed, in the payload.
    assert!(r.recomputation.contains("ring"));
    assert!(r.recomputation.contains("not comparable across keys"));
    assert!(r.ring_capacity > 0);
}

#[test]
fn rows_written_is_null_for_reads_because_a_read_changes_no_rows() {
    let (_d, s) = store();
    for _ in 0..3 {
        s.insert_event(&ev("m")).unwrap();
    }
    s.list_events(Some("m"), 10).unwrap();
    let r = s.db_metrics().unwrap();
    assert_eq!(
        stat(&r.ops, "events.write").unwrap().rows_written,
        Some(3),
        "rows touched separates 'the query got slower' from 'the table got bigger'"
    );
    assert_eq!(
        stat(&r.ops, "events.read").unwrap().rows_written,
        None,
        "not Some(0) — 'this is not a write' and 'this write did nothing' are different findings"
    );
}

#[test]
fn a_family_that_never_ran_is_omitted_rather_than_rendered_as_zeros() {
    let (_d, s) = store();
    s.insert_event(&ev("m")).unwrap();
    let r = s.db_metrics().unwrap();
    assert!(stat(&r.ops, "events.write").is_some());
    assert!(
        stat(&r.ops, "traces.read").is_none(),
        "a zero p95 for a path that never ran is a number someone will quote"
    );
    assert!(stat(&r.ops, "maintenance").is_none());
    s.maintenance_pass(MaintenanceRequest {
        truncate_wal: false,
        reclaim_pages: 0,
    })
    .unwrap();
    assert!(
        stat(&s.db_metrics().unwrap().ops, "maintenance").is_some(),
        "maintenance is its own family, so 'was that stall at 14:03 us?' is answerable"
    );
}

#[test]
fn percentiles_are_derived_at_read_time_from_a_bounded_ring() {
    let (_d, s) = store();
    let r0 = s.db_metrics().unwrap();
    let cap = r0.ring_capacity;
    for _ in 0..(cap + 50) {
        s.insert_event(&ev("m")).unwrap();
    }
    let o = {
        let r = s.db_metrics().unwrap();
        stat(&r.ops, "events.write").unwrap().clone()
    };
    assert_eq!(o.count as usize, cap + 50, "the COUNT is cumulative");
    assert_eq!(
        o.sampled, cap,
        "the percentiles' sample is bounded by the ring — they describe recent behaviour, and the \
         report says so"
    );
    assert!(o.p95_ms >= o.p50_ms);
    assert!(o.max_ms >= o.p95_ms, "max is cumulative, p95 is windowed");
    assert!(o.mean_ms > 0.0);
}

#[test]
fn the_instrument_does_not_use_the_database() {
    let (_d, s) = store();
    let tables = |s: &SqliteStore| -> BTreeSet<String> {
        s.storage_report()
            .unwrap()
            .objects
            .into_iter()
            .filter(|o| o.kind == "table")
            .map(|o| o.name)
            .collect()
    };
    let before = tables(&s);
    for _ in 0..200 {
        s.insert_event(&ev("m")).unwrap();
        s.list_events(Some("m"), 5).unwrap();
    }
    let after = tables(&s);
    assert_eq!(
        before, after,
        "metrics that wrote to a metrics table would double every measured operation and contend \
         for the very locks being measured"
    );
    assert!(
        !after.iter().any(|t| t.contains("metric")),
        "no metrics table exists: {after:?}"
    );
    // And the numbers are there anyway.
    assert_eq!(
        stat(&s.db_metrics().unwrap().ops, "events.write")
            .unwrap()
            .count,
        200
    );
}
