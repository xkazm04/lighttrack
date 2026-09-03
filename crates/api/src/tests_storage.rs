//! The disk/ops surface and the quiet-window gate.
//!
//! Two halves: the wired `GET /v1/storage/status` (auth, and that the growth an operator is being
//! asked to live with is actually *visible* there), and the gate's escalation ladder as a pure
//! decision — the part with the interesting failure modes, which is a gate that never opens, a gate
//! that always opens, or a hard bound that is quietly a wall clock in disguise.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt; // oneshot

use lighttrack_store::Store;

use crate::redact::Redactor;
use crate::storage::{decide, ActivityGauge, Gate, Rung, SweepConfig};
use crate::tests_ingest::{make_key, setup};

async fn get(app: &axum::Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn cfg() -> SweepConfig {
    SweepConfig {
        interval: Duration::from_secs(300),
        min_interval: Duration::from_secs(900),
        stale_after: Duration::from_secs(3_600),
        wal_hard_bytes: 64 * 1024 * 1024,
    }
}

// ── the operator surface ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_disk_surface_is_admin_only() {
    let (state, store) = setup(Redactor::off());
    let project_key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    // It names the database path, every table's size and the process's internal latency profile.
    // That is an operational X-ray, not a tenant-scoped read.
    let (status, _) = get(&app, &project_key, "/v1/storage/status").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = get(&app, "admin-secret", "/v1/storage/status").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_growth_an_operator_must_live_with_is_visible_where_they_look() {
    let (state, store) = setup(Redactor::off());
    for _ in 0..5 {
        // Any write; the point is that the tables are accounted, not what is in them.
        store
            .create_project(&lighttrack_core::Project {
                id: lighttrack_core::new_id(),
                name: "p".into(),
                enabled: true,
                redaction: lighttrack_core::Redaction::None,
                collective_opt_in: false,
                require_trusted_judge: false,
                archived_at: None,
                created_at: chrono::Utc::now(),
            })
            .unwrap();
    }
    let app = crate::build_router(state);
    let (status, body) = get(&app, "admin-secret", "/v1/storage/status").await;
    assert_eq!(status, StatusCode::OK);

    let storage = &body["storage"];
    assert!(storage["db_bytes"].as_u64().unwrap() > 0);
    let objects = storage["objects"].as_array().unwrap();
    assert!(
        objects.iter().any(|o| o["name"] == "events"),
        "the per-table accounting reaches the surface"
    );
    // The retention stance is served WITH the disk figures, not filed in a doc the reader of a
    // number will not have open.
    let retention = storage["retention"].as_str().unwrap();
    assert!(retention.contains("retention deliberately unbounded"));
    assert!(retention.contains("2026-08-24"));
    // And every byte figure carries what it means.
    assert!(storage["bytes_predicate"]
        .as_str()
        .unwrap()
        .contains("pages allocated"));

    // The store's own latency, on the same surface, so "big AND degrading" is one look.
    let ops = body["db_metrics"]["ops"].as_array().unwrap();
    assert!(!ops.is_empty());
    assert!(ops
        .iter()
        .all(|o| o["slow_over_ms"].as_f64().unwrap() > 0.0));

    // Maintenance: a store whose sweep has only ever deferred must be distinguishable from one with
    // no sweep at all, so the deferral count is first-class and "never run" is a null, not a zero.
    let m = &body["maintenance"];
    assert!(m["passes_deferred"].is_number());
    assert!(m["last_run"].is_null(), "no pass has run in this fixture");
    assert!(m["sweep"].as_str().unwrap().contains("test fixture"));
}

#[test]
fn the_activity_gauge_returns_to_zero_even_when_a_handler_unwinds() {
    let g = Arc::new(ActivityGauge::default());
    assert_eq!(g.read(), 0);
    {
        let _a = g.enter_for_test();
        assert_eq!(g.read(), 1);
        let _b = g.enter_for_test();
        assert_eq!(g.read(), 2);
    }
    assert_eq!(g.read(), 0, "tokens decrement on drop");

    // A panicking handler must not leave the gauge permanently busy: that would switch maintenance
    // off forever, silently, which is the hardest failure here to notice.
    let g2 = g.clone();
    let _ = std::panic::catch_unwind(move || {
        let _t = g2.enter_for_test();
        panic!("handler exploded");
    });
    assert_eq!(g.read(), 0);
}

// ── the gate and the ladder ───────────────────────────────────────────────────────────────────

#[test]
fn the_gate_needs_both_conditions_not_either() {
    let c = cfg();
    // Idle, but the interval has not elapsed: deferred. Without this half, an idle instance runs a
    // pass on every evaluation — idle detection turned into a busy loop.
    let d = decide(&c, 0, Duration::from_secs(60), 0, 0.0, 0);
    assert!(matches!(d, Gate::Defer { .. }), "{d:?}");
    if let Gate::Defer { trigger } = d {
        assert!(trigger.contains("minimum interval"));
    }

    // The interval has elapsed, but work is in flight: also deferred. Without THIS half the gate is
    // a wall-clock timer, which is guaranteed to fire mid-request eventually.
    let d = decide(&c, 3, Duration::from_secs(1_000), 0, 0.0, 0);
    assert!(matches!(d, Gate::Defer { .. }), "{d:?}");

    // Both: the preferred rung.
    match decide(&c, 0, Duration::from_secs(1_000), 0, 0.0, 0) {
        Gate::Run { rung, chunk, .. } => {
            assert_eq!(rung, Rung::Quiet);
            assert!(chunk >= 32);
        }
        d => panic!("a quiet window with the interval elapsed must open the gate: {d:?}"),
    }
}

#[test]
fn deferral_does_not_go_on_forever() {
    let c = cfg();
    // Light traffic, and no true quiet window since well past the staleness bound: accept "quieter"
    // with a reduced chunk rather than deferring until the disk decides for us.
    let (rung, chunk) = match decide(&c, 1, Duration::from_secs(7_200), 0, 0.0, 0) {
        Gate::Run { rung, chunk, .. } => (rung, chunk),
        d => panic!("the staleness rung never opened: {d:?}"),
    };
    assert_eq!(rung, Rung::Quieter);
    let full = match decide(&c, 0, Duration::from_secs(7_200), 0, 0.0, 0) {
        Gate::Run { chunk, .. } => chunk,
        d => panic!("{d:?}"),
    };
    assert!(
        chunk < full,
        "the quieter rung runs a SMALLER chunk ({chunk} vs {full}) — it is politeness, not \
         permission to take the machine"
    );

    // But heavy traffic past the staleness bound still defers: "quieter" is not "regardless".
    assert!(matches!(
        decide(&c, 12, Duration::from_secs(7_200), 0, 0.0, 0),
        Gate::Defer { .. }
    ));
}

#[test]
fn the_hard_bound_is_a_harm_not_a_clock() {
    let c = cfg();
    // A busy instance, a fresh pass — every politeness condition says no — and the journal is over
    // its bound. It runs anyway, and the record says why in bytes.
    let (rung, trigger) = match decide(&c, 40, Duration::from_secs(1), 80 * 1024 * 1024, 0.0, 0) {
        Gate::Run { rung, trigger, .. } => (rung, trigger),
        d => panic!("the hard bound did not escalate: {d:?}"),
    };
    assert_eq!(rung, Rung::Escalated);
    assert!(trigger.contains("MiB"), "{trigger}");
    assert!(
        !trigger.contains("week") && !trigger.contains("days"),
        "the hard bound must be stated as the harm, not as elapsed time: {trigger}"
    );

    // Same for reclaimable space — but only when it is worth a pass. A quarter of a tiny file is not.
    assert!(matches!(
        decide(&c, 40, Duration::from_secs(1), 0, 0.40, 64 * 1024 * 1024),
        Gate::Run {
            rung: Rung::Escalated,
            ..
        }
    ));
    assert!(matches!(
        decide(&c, 40, Duration::from_secs(1), 0, 0.40, 200 * 1024),
        Gate::Defer { .. }
    ));
}
