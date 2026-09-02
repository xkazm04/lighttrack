//! The store's long lane: sustained ingest under read load, with quiet-window maintenance running
//! alongside, judged against criteria that were declared before the run.
//!
//! Until 2026-08-24 this repository had no lane on any clock. The one load harness that existed
//! (`crates/store/src/sqlite/bench.rs`) asserted nothing, declared no bounds, produced no artifact,
//! and was `#[ignore]`d — so nothing anywhere could notice a store that got slower over hours, and
//! the only way to find out was for a self-hosted user to feel it.
//!
//! ## A certification, not a gate
//!
//! Long lanes answer questions only time and pressure can ask: does latency hold its *shape* under
//! sustained load, does the file grow in proportion to the data, does the journal stay bounded. None
//! of those is a property of any single change, so blocking a merge on an hours-long run would
//! destroy the merge cadence without improving the certification. This lane therefore runs on its own
//! clock — `.github/workflows/soak.yml`, nightly and on demand — and its unit of value is the trend
//! across runs, not one verdict.
//!
//! Two modes, and the difference is honest rather than cosmetic:
//!
//! * **per-change (the default, seconds).** `cargo test --workspace` runs the lane briefly and
//!   asserts the HARNESS is alive: that it measures, that it produces an artifact, and that it fires
//!   on a planted defect. It does *not* enforce the timing bounds, because a shared runner's noise
//!   would make them a coin flip on a blocking gate. This is deliberately not "skip": a lane wired in
//!   and never exercised is the failure mode that hides in plain sight — every failure after the
//!   first is wallpaper, and a lane with a 100% historical failure rate is an unbuilt lane wearing a
//!   gate's clothes.
//! * **certification (`LIGHTTRACK_SOAK_ENFORCE=1`, minutes).** The nightly run. Every criterion in
//!   `docs/harness/soak-criteria.json` is enforced, and the artifact is uploaded so the sequence of
//!   artifacts *is* the lane's dashboard. A regression that stays inside the bound is still a
//!   regression; the trend line catches what any single verdict forgives.
//!
//! ## Lane health: earned green, planted red
//!
//! Every run does both halves. The good configuration must satisfy every criterion (earned green),
//! and a second run with a deliberate latency injection in its second half must FAIL — and fail on
//! `write_p95_drift_ratio` specifically (planted red). A lane that has never been observed to fail
//! for cause is indistinguishable from a lane that cannot fail, and a lane that fails for the wrong
//! reason certifies nothing at all.
//!
//! ## Env
//!
//! * `LIGHTTRACK_SOAK_SECS` — seconds per phase (default 4; the nightly sets 300).
//! * `LIGHTTRACK_SOAK_ENFORCE` — `1` to enforce the timing criteria, not only harness liveness.
//! * `LIGHTTRACK_SOAK_ARTIFACT` — where to write the run artifact (default `target/soak.json`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{json, Value};

use lighttrack_core::{new_id, LlmEvent, Operation, Status, TokenUsage};
use lighttrack_store::{MaintenanceRequest, SqliteStore, Store};

/// The criteria are read from the committed file, not restated here. `include_str!` so a missing or
/// renamed file is a BUILD error rather than a lane that quietly certifies nothing.
const CRITERIA: &str = include_str!("../../../docs/harness/soak-criteria.json");

/// Buckets the run is divided into for the trend criterion. Even, so the halves are comparable.
const BUCKETS: usize = 8;
/// How often the maintenance thread evaluates, mirroring the API's quiet-window sweep.
const MAINTENANCE_EVERY: Duration = Duration::from_millis(500);
/// Journal size past which the maintenance pass truncates rather than passively checkpoints.
const WAL_TRUNCATE_OVER: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The build under test.
    KnownGood,
    /// A build with a defect this lane CLAIMS to catch: latency that grows through the run's second
    /// half. Nobody schedules this half, which is exactly why it is here.
    PlantedDrift,
}

struct Measured {
    events: u64,
    errors: u64,
    reads: u64,
    write_p95_ms: f64,
    first_half_p95_ms: f64,
    second_half_p95_ms: f64,
    drift_ratio: f64,
    db_bytes: u64,
    db_bytes_per_event: f64,
    wal_bytes_max: u64,
    /// Per-bucket p95, the series the artifact carries.
    bucket_p95_ms: Vec<f64>,
    maintenance_passes: u64,
    pages_checkpointed: u64,
    seconds: f64,
}

fn ev(project: &str, payload_bytes: usize) -> LlmEvent {
    let blob: String = "x".repeat(payload_bytes);
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
        name: Some("soak".into()),
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
        input: Some(Value::String(blob.clone())),
        output: Some(Value::String(blob)),
        tags: vec![],
        source: Some("soak".into()),
        metadata: Default::default(),
    }
}

fn p95(sorted: &[u64]) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = (sorted.len() * 95 / 100).min(sorted.len() - 1);
    sorted[i] as f64 / 1000.0
}

/// Run the lane once.
fn run_lane(secs: u64, payload_bytes: usize, readers: usize, mode: Mode) -> Measured {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteStore::open(dir.path().join("soak.db")).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let total = Duration::from_secs(secs.max(1));
    let bucket_len = total / BUCKETS as u32;

    // Readers: the dashboard shape, running the whole time so the writer really is competing.
    let read_count = Arc::new(AtomicU64::new(0));
    let readers: Vec<_> = (0..readers)
        .map(|_| {
            let (s, stop, n) = (store.clone(), stop.clone(), read_count.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = s.cost_summary(Some("soak"));
                    let _ = s.list_events(Some("soak"), 200);
                    n.fetch_add(2, Ordering::Relaxed);
                }
            })
        })
        .collect();

    // The quiet-window maintenance pass, running alongside exactly as the API's sweep would. The
    // journal-size criterion is meaningless without it: this lane certifies the pair, not the store
    // in isolation.
    let wal_max = Arc::new(AtomicU64::new(0));
    let passes = Arc::new(AtomicU64::new(0));
    let checkpointed = Arc::new(AtomicU64::new(0));
    let maint = {
        let (s, stop) = (store.clone(), stop.clone());
        let (wal_max, passes, checkpointed) =
            (wal_max.clone(), passes.clone(), checkpointed.clone());
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(MAINTENANCE_EVERY);
                let wal = s
                    .storage_report()
                    .ok()
                    .and_then(|r| r.wal_bytes)
                    .unwrap_or(0);
                wal_max.fetch_max(wal, Ordering::Relaxed);
                if let Ok(p) = s.maintenance_pass(MaintenanceRequest {
                    truncate_wal: wal >= WAL_TRUNCATE_OVER,
                    reclaim_pages: 64,
                }) {
                    passes.fetch_add(1, Ordering::Relaxed);
                    checkpointed.fetch_add(p.pages_checkpointed, Ordering::Relaxed);
                }
            }
        })
    };

    // The writer. One, and that is a fact rather than an approximation: the backend serialises
    // writes behind a single connection by design, so more writers would measure queueing.
    let per_bucket: Arc<Mutex<Vec<Vec<u64>>>> = Arc::new(Mutex::new(vec![Vec::new(); BUCKETS]));
    let mut errors = 0u64;
    let mut events = 0u64;
    let t0 = Instant::now();
    while t0.elapsed() < total {
        let e = ev("soak", payload_bytes);
        let b =
            ((t0.elapsed().as_nanos() / bucket_len.as_nanos().max(1)) as usize).min(BUCKETS - 1);
        let t = Instant::now();
        // The plant: a defect the lane CLAIMS to catch — write latency that grows through the run's
        // second half. It is injected INSIDE the measured window, where a real regression would
        // live. (First attempt put the sleep after the measurement, which slowed the run's
        // throughput and moved the measured latency not at all — the planted-red assertion caught
        // it immediately, which is the assertion earning its place on its first day.)
        //
        // The injection starts AFTER the second half's first bucket, deliberately: that bucket is
        // the trend criterion's denominator, so leaving it clean means the ratio is
        // (degraded / honest baseline) rather than (degraded / slightly-degraded). A first attempt
        // injected from the first second-half bucket onward and the lane's own planted-red check
        // caught it going quiet under ambient machine noise — a marginal planted red is a planted
        // red that will be green on somebody's busy afternoon.
        if mode == Mode::PlantedDrift && b > BUCKETS / 2 {
            let over = (b - BUCKETS / 2) as u64;
            thread::sleep(Duration::from_millis(20 * over));
        }
        match store.insert_event_checked(&e) {
            Ok(_) => events += 1,
            Err(_) => errors += 1,
        }
        let took = t.elapsed();
        per_bucket.lock().unwrap()[b].push(took.as_micros().min(u128::from(u64::MAX)) as u64);
    }
    let seconds = t0.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        let _ = r.join();
    }
    let _ = maint.join();

    let buckets = per_bucket.lock().unwrap().clone();
    let bucket_p95_ms: Vec<f64> = buckets
        .iter()
        .map(|b| {
            let mut v = b.clone();
            v.sort_unstable();
            p95(&v)
        })
        .collect();
    let mut all: Vec<u64> = buckets.iter().flatten().copied().collect();
    all.sort_unstable();
    let half = |range: std::ops::Range<usize>| -> f64 {
        let mut v: Vec<u64> = buckets[range].iter().flatten().copied().collect();
        v.sort_unstable();
        p95(&v)
    };
    let first = half(0..BUCKETS / 2);
    let second = half(BUCKETS / 2..BUCKETS);
    // The slope is measured WITHIN the second half — the final bucket against the second half's
    // first bucket — so the opening bucket's cache warm-up cannot masquerade as degradation. That
    // is the distinction the trend criterion exists to make; comparing half against half folds
    // warm-up into the numerator and produces a ratio nobody can act on.
    let slope_from = bucket_p95_ms[BUCKETS / 2];
    let slope_to = bucket_p95_ms[BUCKETS - 1];

    let report = store.storage_report().unwrap();
    Measured {
        events,
        errors,
        reads: read_count.load(Ordering::Relaxed),
        write_p95_ms: p95(&all),
        first_half_p95_ms: first,
        second_half_p95_ms: second,
        // Guarded: a zero denominator would make the ratio meaningless rather than infinite.
        drift_ratio: if slope_from > 0.0 {
            slope_to / slope_from
        } else {
            1.0
        },
        db_bytes: report.db_bytes,
        db_bytes_per_event: if events > 0 {
            report.db_bytes as f64 / events as f64
        } else {
            f64::INFINITY
        },
        wal_bytes_max: wal_max.load(Ordering::Relaxed),
        bucket_p95_ms,
        maintenance_passes: passes.load(Ordering::Relaxed),
        pages_checkpointed: checkpointed.load(Ordering::Relaxed),
        seconds,
    }
}

/// A criterion's declared bound, read from the committed file. Panics loudly on a missing key: a
/// criterion that silently defaults is a criterion nobody declared.
fn bound(criteria: &Value, key: &str) -> f64 {
    criteria["criteria"][key]["bound"]
        .as_f64()
        .unwrap_or_else(|| panic!("soak-criteria.json declares no bound for `{key}`"))
}

/// Judge the measurements against the declared criteria. Returns the failures, each naming the
/// criterion, the bound and the observed value — a verdict of "failed" with no number attached is
/// not a verdict.
fn judge(criteria: &Value, m: &Measured) -> Vec<String> {
    let mut out = Vec::new();
    let check = |out: &mut Vec<String>, key: &str, observed: f64| {
        let b = bound(criteria, key);
        if observed > b {
            out.push(format!(
                "{key}: {observed:.3} exceeds the declared bound {b:.3} — {}",
                criteria["criteria"][key]["predicate"]
                    .as_str()
                    .unwrap_or("(no predicate declared)")
            ));
        }
    };
    check(&mut out, "max_errors", m.errors as f64);
    check(&mut out, "write_p95_ms", m.write_p95_ms);
    check(&mut out, "write_p95_drift_ratio", m.drift_ratio);
    check(&mut out, "db_bytes_per_event", m.db_bytes_per_event);
    check(&mut out, "wal_bytes_max", m.wal_bytes_max as f64);
    out
}

fn artifact(
    criteria: &Value,
    good: &Measured,
    bad: &Measured,
    failures: &[String],
    planted: &[String],
    enforced: bool,
) -> Value {
    json!({
        "lane": criteria["lane"],
        "run_at": Utc::now().to_rfc3339(),
        "mode": if enforced { "certification" } else { "harness-liveness (timing bounds reported, not enforced)" },
        "duration_secs": good.seconds,
        "workload": criteria["workload"],
        "criteria": criteria["criteria"],
        "measured": {
            "events": good.events,
            "errors": good.errors,
            "reads": good.reads,
            "write_p95_ms": good.write_p95_ms,
            "first_half_p95_ms": good.first_half_p95_ms,
            "second_half_p95_ms": good.second_half_p95_ms,
            "write_p95_drift_ratio": good.drift_ratio,
            "db_bytes": good.db_bytes,
            "db_bytes_per_event": good.db_bytes_per_event,
            "wal_bytes_max": good.wal_bytes_max,
            "maintenance_passes": good.maintenance_passes,
            "pages_checkpointed": good.pages_checkpointed,
            "bucket_p95_ms": good.bucket_p95_ms,
        },
        "verdict": if failures.is_empty() { "passed" } else { "failed" },
        "failures": failures,
        "lane_health": {
            "green_on_known_good": failures.is_empty(),
            "red_on_known_bad": !planted.is_empty(),
            "planted_defect": "latency injected into the write path through the run's second half",
            "planted_defect_failures": planted,
            "planted_defect_measured": {
                "write_p95_ms": bad.write_p95_ms,
                "write_p95_drift_ratio": bad.drift_ratio,
                "bucket_p95_ms": bad.bucket_p95_ms,
                "events": bad.events,
            },
        },
        "note": "Every number above carries its predicate in `criteria`. The unit of value is the \
                 sequence of these artifacts, not this one verdict: a regression that stays inside \
                 its bound is still a regression, and only the trend line catches it."
    })
}

#[test]
fn store_soak_lane() {
    let criteria: Value = serde_json::from_str(CRITERIA).expect("soak-criteria.json is parseable");
    let secs: u64 = std::env::var("LIGHTTRACK_SOAK_SECS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(4);
    let enforce = std::env::var("LIGHTTRACK_SOAK_ENFORCE").is_ok_and(|v| v == "1");
    let payload = criteria["workload"]["payload_bytes"].as_u64().unwrap() as usize;
    let readers = criteria["workload"]["readers"].as_u64().unwrap() as usize;

    let good = run_lane(secs, payload, readers, Mode::KnownGood);
    let failures = judge(&criteria, &good);

    // Planted red, every run. Nobody schedules this half — which is exactly why a lane that has
    // never been observed to fail for cause is indistinguishable from one that cannot fail.
    let bad = run_lane(secs, payload, readers, Mode::PlantedDrift);
    let planted = judge(&criteria, &bad);

    let art = artifact(&criteria, &good, &bad, &failures, &planted, enforce);
    // Default under cargo's own per-test scratch directory: it is gitignored, it exists, and it does
    // not depend on which directory cargo happened to run the test from. The workflow passes an
    // explicit path so the artifact can be uploaded.
    let path = std::env::var("LIGHTTRACK_SOAK_ARTIFACT")
        .unwrap_or_else(|_| format!("{}/soak.json", env!("CARGO_TARGET_TMPDIR")));
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, serde_json::to_string_pretty(&art).unwrap()).unwrap();
    println!("{}", serde_json::to_string_pretty(&art).unwrap());
    println!("soak artifact written to {path}");

    // --- assertions that hold in EVERY mode: the lane is alive and it can go red ---

    assert!(
        good.events > 0 && good.reads > 0,
        "the lane measured nothing — a harness that certifies an empty run is worse than no harness"
    );
    assert!(
        good.maintenance_passes > 0,
        "no maintenance pass ran during the lane, so the journal criterion certifies nothing"
    );
    assert_eq!(
        good.errors, 0,
        "committed work was lost or refused during a plain ingest run"
    );
    assert!(
        !planted.is_empty(),
        "PLANTED RED FAILED: a deliberate latency injection through the second half did not trip a \
         single criterion. The lane cannot see the defect it claims to catch."
    );
    assert!(
        planted
            .iter()
            .any(|f| f.starts_with("write_p95_drift_ratio")),
        "the planted defect tripped the lane, but on the wrong criterion — a lane that fails for \
         the wrong reason certifies nothing. Failures were: {planted:?}"
    );

    // --- the timing certification itself, only when this run is the certification ---

    if enforce {
        assert!(
            failures.is_empty(),
            "the long lane FAILED its declared criteria:\n  {}",
            failures.join("\n  ")
        );
    } else if !failures.is_empty() {
        // Reported, never enforced here: a shared runner's noise must not turn a blocking gate into
        // a coin flip. The nightly lane is where these are the verdict.
        println!(
            "note: timing criteria not met in the short per-change run (not enforced): {failures:?}"
        );
    }
}
