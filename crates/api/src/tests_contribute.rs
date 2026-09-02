//! End-to-end tests for the contributor side (M22), against a **local axum stub hub**.
//!
//! What these pin, and why a unit test could not: the push is a real outbound HTTP call, so the two
//! properties that matter — *an unchanged digest makes no call at all*, and *whatever the hub said
//! ends up in the ledger* — can only be observed by counting requests at the other end of a socket.
//!
//! The stub hub is a two-route axum app on `127.0.0.1:0` with a hit counter. It answers the shape a
//! real hub answers (`{ accepted, contributor_id }`), so the ack path is exercised for real rather
//! than mocked.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::{delete, post};
use axum::{Json, Router};
use chrono::Utc;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use lighttrack_core::{new_id, Benchmark, BenchmarkCase, BenchmarkRun, Project, Redaction};
use lighttrack_store::{SqliteStore, Store};

use crate::state::AppState;

/// A stub hub's observed traffic.
#[derive(Default)]
struct HubHits {
    ingest: AtomicUsize,
    withdraw: AtomicUsize,
}

struct StubHub {
    base: String,
    hits: Arc<HubHits>,
    /// Held so the server task lives as long as the test.
    _task: tokio::task::JoinHandle<()>,
}

/// Start a hub on an ephemeral port that answers `status` to every ingest.
async fn stub_hub(status: StatusCode) -> StubHub {
    let hits = Arc::new(HubHits::default());
    let app =
        Router::new()
            .route(
                "/v1/collective/ingest",
                post(
                    |State((hits, status)): State<(Arc<HubHits>, StatusCode)>,
                     Json(d): Json<Value>| async move {
                        hits.ingest.fetch_add(1, Ordering::SeqCst);
                        let n = d["entries"].as_array().map(Vec::len).unwrap_or(0);
                        (
                            status,
                            Json(json!({ "accepted": n, "contributor_id": "c-hubside" })),
                        )
                    },
                ),
            )
            .route(
                "/v1/collective/contribution",
                delete(
                    |State((hits, _)): State<(Arc<HubHits>, StatusCode)>| async move {
                        hits.withdraw.fetch_add(1, Ordering::SeqCst);
                        Json(json!({ "contributor_id": "c-hubside", "deleted": 3 }))
                    },
                ),
            )
            .with_state((hits.clone(), status));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    StubHub {
        base: format!("http://{addr}"),
        hits,
        _task: task,
    }
}

/// An instance with one consenting project holding a benchmark run, so its digest is non-empty.
fn setup_contributor() -> (AppState, Arc<SqliteStore>) {
    let (state, store) = crate::tests_collective::setup_for_contribution();
    let project = Project {
        id: "proj-open".into(),
        name: "proj-open".into(),
        enabled: true,
        redaction: Redaction::None,
        collective_opt_in: true,
        archived_at: None,
        created_at: Utc::now(),
    };
    store.create_project(&project).unwrap();
    add_run(store.as_ref(), "proj-open", "public-model", 0.9);
    (state, store)
}

fn add_run(store: &SqliteStore, project: &str, model: &str, score: f64) {
    let b = Benchmark {
        id: new_id(),
        project_id: project.into(),
        name: "qa bench".into(),
        rubric: "is it right".into(),
        judge_model: "haiku".into(),
        target: json!({ "provider": "anthropic", "model": model }),
        dataset_ref: None,
        rubric_id: None,
        dataset: vec![BenchmarkCase {
            input: "2+2".into(),
            expected: None,
            output: None,
        }],
        baseline_score: None,
        created_at: Utc::now(),
    };
    store.create_benchmark(&b).unwrap();
    store
        .create_benchmark_run(&BenchmarkRun {
            id: new_id(),
            benchmark_id: b.id.clone(),
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            n_cases: 10,
            mean_score: Some(score),
            pass_rate: Some(score),
            cost_usd: 0.1,
            status: "passed".into(),
            p50_latency_ms: Some(100),
            p95_latency_ms: Some(200),
            total_tokens: Some(100),
            report: Value::Null,
        })
        .unwrap();
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    let req = match body {
        Some(b) => req.body(Body::from(b.to_string())).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, v)
}

async fn contribute(app: &Router, hub: &str, force: bool) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        "/v1/collective/contribute",
        Some(json!({ "hub": hub, "force": force })),
    )
    .await
}

/// The headline behaviour: a changed digest is pushed and recorded; an unchanged one makes **no
/// HTTP call at all** — which is what makes a `Contribute` schedule safe against a hub's
/// `min_interval`.
#[tokio::test]
async fn an_unchanged_digest_is_skipped_and_a_changed_one_is_pushed() {
    let hub = stub_hub(StatusCode::OK).await;
    let (state, store) = setup_contributor();
    let app = crate::build_router(state);

    let (status, body) = contribute(&app, &hub.base, false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcome"], "sent", "{body}");
    assert_eq!(body["entries"], 1, "{body}");
    assert_eq!(body["projects_included"], 1, "{body}");
    assert_eq!(
        hub.hits.ingest.load(Ordering::SeqCst),
        1,
        "one push went out"
    );
    assert_eq!(
        body["ack"]["contributor_id"], "c-hubside",
        "the hub's ack is returned verbatim: {body}"
    );

    // Second call, nothing changed: the gate must stop it BEFORE the socket.
    let (status, body) = contribute(&app, &hub.base, false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcome"], "skipped", "{body}");
    assert!(
        body["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("unchanged"),
        "the skip says why: {body}"
    );
    assert_eq!(
        hub.hits.ingest.load(Ordering::SeqCst),
        1,
        "an unchanged digest must make NO HTTP call — this is the whole point of the hash gate"
    );

    // …and a skip writes no ledger row: the ledger records what left the building.
    let (_, ledger) = call(&app, "GET", "/v1/collective/contributions", None).await;
    assert_eq!(ledger.as_array().map(Vec::len), Some(1), "{ledger}");

    // A new measurement changes the digest, so the next push goes out.
    add_run(store.as_ref(), "proj-open", "public-model", 0.4);
    let (status, body) = contribute(&app, &hub.base, false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["outcome"], "sent",
        "a changed digest is pushed: {body}"
    );
    assert_eq!(hub.hits.ingest.load(Ordering::SeqCst), 2);

    let (_, ledger) = call(&app, "GET", "/v1/collective/contributions", None).await;
    let rows = ledger.as_array().expect("array");
    assert_eq!(rows.len(), 2, "{ledger}");
    assert_ne!(
        rows[0]["digest_sha256"], rows[1]["digest_sha256"],
        "the two pushes carried different digests"
    );
    // Newest first, and the ledger keeps the counts, never the digest body.
    assert_eq!(rows[0]["status"], "sent");
    assert_eq!(rows[0]["entries_count"], 1);
    assert!(
        !ledger.to_string().contains("public-model"),
        "the ledger stores a HASH and counts, never the digest body: {ledger}"
    );
}

/// `force=true` is the escape hatch for "the hub lost its database" — the only case where sending
/// a byte-identical body again is the right thing to do.
#[tokio::test]
async fn force_pushes_an_unchanged_digest_anyway() {
    let hub = stub_hub(StatusCode::OK).await;
    let (state, _) = setup_contributor();
    let app = crate::build_router(state);

    contribute(&app, &hub.base, false).await;
    let (_, body) = contribute(&app, &hub.base, true).await;
    assert_eq!(body["outcome"], "sent", "{body}");
    assert_eq!(hub.hits.ingest.load(Ordering::SeqCst), 2);
}

/// A hub that answers and refuses is recorded as `rejected`, with its answer — and the gate does
/// **not** treat it as sent, so the next attempt actually goes out.
#[tokio::test]
async fn a_refusal_is_recorded_and_does_not_arm_the_gate() {
    let hub = stub_hub(StatusCode::TOO_MANY_REQUESTS).await;
    let (state, _) = setup_contributor();
    let app = crate::build_router(state);

    let (status, body) = contribute(&app, &hub.base, false).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a refused push is not an API error: {body}"
    );
    assert_eq!(body["outcome"], "rejected", "{body}");

    let (_, ledger) = call(&app, "GET", "/v1/collective/contributions", None).await;
    assert_eq!(ledger[0]["status"], "rejected", "{ledger}");

    // The hub does NOT have this digest, so the identical next attempt must be sent, not skipped.
    let (_, body) = contribute(&app, &hub.base, false).await;
    assert_eq!(
        body["outcome"], "rejected",
        "a rejected push must not arm the unchanged-digest gate: {body}"
    );
    assert_eq!(hub.hits.ingest.load(Ordering::SeqCst), 2);
}

/// An empty digest is not a contribution: pushing one would ask the hub to replace this source's
/// whole set with nothing, i.e. a silent withdrawal.
#[tokio::test]
async fn nothing_above_the_floor_is_a_skip_not_an_empty_push() {
    let hub = stub_hub(StatusCode::OK).await;
    let (state, _) = crate::tests_collective::setup_for_contribution();
    let app = crate::build_router(state);

    let (status, body) = contribute(&app, &hub.base, false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcome"], "skipped", "{body}");
    assert_eq!(body["entries"], 0);
    assert_eq!(
        hub.hits.ingest.load(Ordering::SeqCst),
        0,
        "an empty digest must never be pushed"
    );
}

/// A hub URL that is not an absolute http(s) URL is refused at the door, not attempted.
#[tokio::test]
async fn a_relative_hub_is_a_400() {
    let (state, _) = setup_contributor();
    let app = crate::build_router(state);
    let (status, body) = contribute(&app, "hub.example", false).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("absolute"));
}

/// `?all=1` walks the ledger and asks each hub it can name to delete our contribution. The hub is
/// named through `?hub=`, because the ledger stores an opaque hash rather than an address.
#[tokio::test]
async fn withdraw_all_covers_every_ledgered_hub_it_can_name() {
    let hub = stub_hub(StatusCode::OK).await;
    let (state, _) = setup_contributor();
    let app = crate::build_router(state);

    contribute(&app, &hub.base, false).await;
    let uri = format!(
        "/v1/collective/contribution?all=1&hub={}",
        urlencode(&hub.base)
    );
    let (status, body) = call(&app, "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(hub.hits.withdraw.load(Ordering::SeqCst), 1, "{body}");
    assert_eq!(
        body["withdrawn"].as_array().map(Vec::len),
        Some(1),
        "{body}"
    );
    assert_eq!(body["withdrawn"][0]["ok"], true, "{body}");
    assert_eq!(
        body["unresolved"].as_array().map(Vec::len),
        Some(0),
        "the hub was named, so nothing is unresolved: {body}"
    );

    // Without the name, the hub is REPORTED as unresolved rather than silently skipped — a
    // withdrawal that quietly covered less than it claimed is the failure mode this avoids.
    let (_, body) = call(&app, "DELETE", "/v1/collective/contribution?all=1", None).await;
    assert_eq!(
        body["withdrawn"].as_array().map(Vec::len),
        Some(0),
        "{body}"
    );
    assert_eq!(
        body["unresolved"].as_array().map(Vec::len),
        Some(1),
        "{body}"
    );
    assert_eq!(
        hub.hits.withdraw.load(Ordering::SeqCst),
        1,
        "nothing more was contacted"
    );
}

/// The plain `DELETE /v1/collective/contribution` is unchanged: it is still the HUB-side
/// self-withdrawal, not the fan-out.
#[tokio::test]
async fn withdraw_without_all_is_still_the_hub_side_delete() {
    let (state, _) = setup_contributor();
    let app = crate::build_router(state);
    let (status, body) = call(&app, "DELETE", "/v1/collective/contribution", None).await;
    // Still `resolve_contributor`, which refuses a keyless caller — the pre-M22 behaviour exactly.
    // The fan-out would have answered 200 with a `withdrawn` array, so this is the evidence that
    // adding `?all=1` did not quietly change what the bare route does.
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body.get("withdrawn").is_none(),
        "the bare route must not take the fan-out path: {body}"
    );
}

fn urlencode(s: &str) -> String {
    s.replace(':', "%3A").replace('/', "%2F")
}
