//! The route-level acceptance cases: comparability, the declared contract row, the surface that
//! takes the route out, and the GUARD that `GET .../gate` did not move.

use axum::http::StatusCode;
use lighttrack_core::{Benchmark, BenchmarkRun, Scope, Score};
use lighttrack_store::{Capabilities, SqliteStore, Store, Surface};
use serde_json::{json, Value};

use super::*;
use crate::benchmarks::decide_gate;
use crate::capabilities;
use crate::redact::Redactor;
use crate::tests_access_probe::{scoped_key, send};
use crate::tests_ingest::setup;

const ROUTE: &str = "GET /v1/benchmarks/{id}/regression";

fn row() -> &'static lighttrack_contract::Endpoint {
    lighttrack_contract::endpoints()
        .find(|e| e.id == "benchmark_regression")
        .expect(
            "the new read route MUST be declared in lighttrack_contract::endpoints() — \
             access_layer resolves every request against that table and answers 500 for a route \
             wired in main.rs and declared nowhere",
        )
}

fn bench(judge: &str) -> Benchmark {
    serde_json::from_value(json!({
        "id": "b1", "project_id": "p-reg", "name": "b", "rubric": "r",
        "judge_model": judge, "baseline_score": 0.8,
    }))
    .expect("benchmark")
}

fn run(id: &str, judge: &str) -> BenchmarkRun {
    at(id, judge, "2026-01-02T00:00:00.000000000Z")
}

/// Distinct `started_at`s, because "newest first" is the ordering the default `against` walks: two
/// runs stamped the same instant leave it ambiguous.
fn at(id: &str, judge: &str, started: &str) -> BenchmarkRun {
    serde_json::from_value(json!({
        "id": id, "benchmark_id": "b1", "n_cases": 1, "mean_score": 0.9,
        "status": "passed", "started_at": started,
        "finished_at": "2026-01-03T00:01:00.000000000Z",
        "report": { "judge_model": judge },
    }))
    .expect("run")
}

fn case_score(run_id: &str, dim: &str, value: f64) -> Score {
    serde_json::from_value(json!({
        "project_id": "p-reg", "rubric": "r", "kind": "bench_case",
        "value": value, "max": 1.0, "pass": value >= 0.5, "scored_by": "haiku",
        "run_id": run_id, "case_index": 1,
        "detail": { "dimensions": [{ "key": dim, "value": value, "weight": 1.0 }] },
    }))
    .expect("score")
}

/// A benchmark with two finished runs, both scored by `run_judge`, against a benchmark that now
/// judges with `bench_judge` — so passing two different strings stages the comparability refusal.
fn fixture(
    store: &SqliteStore,
    bench_judge: &str,
    run_judge: &str,
) -> (Benchmark, Vec<BenchmarkRun>) {
    let b = bench(bench_judge);
    store.create_benchmark(&b).expect("benchmark");
    let runs = vec![
        at("run-new", run_judge, "2026-01-02T00:00:00.000000000Z"),
        at("run-old", run_judge, "2026-01-01T00:00:00.000000000Z"),
    ];
    store.create_benchmark_run(&runs[1]).expect("old run");
    store.create_benchmark_run(&runs[0]).expect("new run");
    store
        .insert_score(&case_score("run-old", "safety", 0.9))
        .expect("old score");
    store
        .insert_score(&case_score("run-new", "safety", 0.1))
        .expect("new score");
    (b, runs)
}

/// GUARD (coordinator mandate) — the route is a declared row carrying the credential it is served
/// under, so `access_layer::enforce` resolves it rather than answering its "declares no Endpoint
/// row" 500.
#[test]
fn the_route_is_declared_in_the_contract_with_its_credential() {
    let e = row();
    assert_eq!(e.path, "/v1/benchmarks/{id}/regression");
    assert_eq!(e.method, lighttrack_contract::Method::Get);
    assert_eq!(
        e.access,
        lighttrack_contract::Access::Key(lighttrack_contract::KeyScope::Read),
        "a read of a benchmark's own verdicts is served to the credential the gate is"
    );
    assert!(!e.mutating, "reading a regression changes nothing");
    let mcp = e
        .mcp
        .as_ref()
        .expect("the agent that ran the benchmark reads the attribution without a browser");
    assert!(
        mcp.read_only,
        "readOnlyHint — this tool is side-effect-free"
    );
}

/// The other half of the same guard, through the router: the request resolves against the declared
/// row and reaches the handler (which 404s an unknown benchmark) instead of 500ing as undeclared.
#[tokio::test]
async fn the_route_resolves_through_the_access_layer_with_its_declared_credential() {
    let (state, store) = setup(Redactor::off());
    let key = scoped_key(&store, "p-guard", vec![Scope::Read]);
    let app = crate::build_router(state);

    let (status, body) = send(
        &app,
        "GET",
        "/v1/benchmarks/nope/regression?run=r1",
        Some(&key),
    )
    .await;
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        !msg.contains("declares no Endpoint row"),
        "the access layer could not resolve the route: {msg}"
    );
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "resolved and handled: {body}"
    );

    // …and the declared credential is enforced: no key at all is a 401.
    let (status, _) = send(&app, "GET", "/v1/benchmarks/nope/regression?run=r1", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Acceptance case 1, end to end: two finished runs of one benchmark get a per-dimension table,
/// worst drop first, and `against` defaults to the previous finished run.
#[tokio::test]
async fn two_comparable_runs_get_a_per_dimension_table() {
    let (state, store) = setup(Redactor::off());
    let key = scoped_key(&store, "p-reg", vec![Scope::Read]);
    fixture(&store, "haiku", "haiku");
    let app = crate::build_router(state);

    let (status, body) = send(
        &app,
        "GET",
        "/v1/benchmarks/b1/regression?run=run-new&against=run-old",
        Some(&key),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["dimensions"][0]["key"], "safety");
    assert!(
        body["dimensions"][0]["delta"].as_f64().expect("delta") < -0.7,
        "{body}"
    );
    assert_eq!(body["flipped"][0]["case_index"], 1);

    let (status, dflt) = send(
        &app,
        "GET",
        "/v1/benchmarks/b1/regression?run=run-new",
        Some(&key),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        dflt, body,
        "`against` defaults to the previous finished run"
    );
}

/// Acceptance case 4 — the runs were scored by a different instrument than the benchmark now uses,
/// so the comparison is refused with 409 carrying the **same message** `instrument_moved` already
/// produces. The rule is called, not copied.
#[tokio::test]
async fn a_judge_swap_is_a_409_in_the_gates_own_words() {
    let (state, store) = setup(Redactor::off());
    let key = scoped_key(&store, "p-reg", vec![Scope::Read]);
    let (b, runs) = fixture(&store, "opus", "haiku");
    let expected = instrument_moved(&runs[0], &b).expect("the gate's own refusal");
    let app = crate::build_router(state);

    let (status, body) = send(
        &app,
        "GET",
        "/v1/benchmarks/b1/regression?run=run-new&against=run-old",
        Some(&key),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["error"]["message"].as_str().unwrap_or_default(),
        expected,
        "the wording is the gate's, not a second copy of the rule"
    );
}

/// Acceptance case 7, first clause — which surface takes this route out. Its only store read is
/// `list_run_scores`, so it must claim the surface that owns that method; `capabilities::routes`
/// runs the completeness check in both directions over the result.
#[test]
fn the_route_claims_the_surface_that_owns_the_store_read_it_depends_on() {
    let owner = Surface::ALL
        .iter()
        .find(|s| s.methods().contains(&"list_run_scores"))
        .expect("some surface owns list_run_scores");
    assert_eq!(row().surface, owner.as_str());
    // The claim has teeth: a backend refusing that surface publishes this route as one that answers
    // HTTP 501, rather than leaving an operator to find out by calling it.
    let refuses_everything = Capabilities::new("probe", &[], false);
    assert!(
        capabilities::routes::unsupported(&refuses_everything).contains(&ROUTE.to_string()),
        "a backend without {} must publish {ROUTE} as refused",
        owner.as_str()
    );
    // …and the reference backend, which refuses nothing, never reports it refused.
    assert!(
        !capabilities::routes::unsupported(&SqliteStore::manifest()).contains(&ROUTE.to_string())
    );
}

/// Acceptance case 7, second clause — GUARD: `GET .../gate` is byte-identical for all five
/// statuses. `GateResponse` and `decide_gate` are untouched by this card; this pins that.
#[test]
fn the_gate_json_is_unchanged_for_every_status() {
    let b = bench("haiku");
    for (status, want) in [
        (
            "passed",
            json!({"status":"pass","run_id":"r","mean":0.9,"baseline":0.8,"n":1}),
        ),
        (
            "regressed",
            json!({"status":"regressed","run_id":"r","mean":0.9,"baseline":0.8,"n":1}),
        ),
        (
            "no_baseline",
            json!({"status":"no_baseline","run_id":"r","mean":0.9,"baseline":0.8,"n":1}),
        ),
        (
            "partial",
            json!({"status":"partial","run_id":"r","mean":0.9,"baseline":0.8,"n":1}),
        ),
    ] {
        let mut r = run("r", "haiku");
        r.status = status.into();
        let g = decide_gate(&[r], &b, None);
        let got: Value = serde_json::to_value(&g).expect("json");
        assert_eq!(got, want, "{status}");
    }
    let g = decide_gate(&[], &b, None);
    assert_eq!(
        serde_json::to_value(&g).expect("json"),
        json!({"status":"no_runs","baseline":0.8})
    );
}
