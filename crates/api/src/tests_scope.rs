//! M17 end-to-end: a foreign id is **404, not 403**, and the job queue is no longer readable
//! across tenants.
//!
//! These drive the wired router rather than the store, because the property is a property of the
//! stack: the scope is only real if the handler actually passes the principal's, and the whole
//! point of deleting the post-hoc `forbidden(...)` branches is that nothing downstream re-adds a
//! distinguishable answer. A 403 here would be a regression even though it *looks* stricter — it
//! tells a stranger that the id exists.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::Utc;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use lighttrack_core::{new_id, Benchmark, Dataset, JobKind, Rubric};
use lighttrack_store::Store;

use crate::redact::Redactor;
use crate::tests_ingest::{make_key, setup};

async fn get(app: &Router, uri: &str, token: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, v)
}

/// Every read that used to answer 403 on a foreign id now answers 404, with the `not_found`
/// envelope — so the two cases a project key can distinguish are "mine" and "not visible", and
/// never "someone else's".
#[tokio::test]
async fn a_foreign_id_is_not_found_rather_than_forbidden() {
    let (state, store) = setup(Redactor::off());
    let mine = make_key(&store, "proj-mine");
    make_key(&store, "proj-theirs");
    let app = crate::build_router(state.clone());

    let bench = Benchmark {
        id: new_id(),
        project_id: "proj-theirs".into(),
        name: "theirs".into(),
        rubric: "is it right".into(),
        judge_model: "haiku".into(),
        target: json!([{ "provider": "anthropic", "model": "haiku" }]),
        dataset_ref: None,
        rubric_id: None,
        dataset: Vec::new(),
        baseline_score: None,
        created_at: Utc::now(),
    };
    store.create_benchmark(&bench).unwrap();

    let ds = Dataset {
        id: new_id(),
        project_id: "proj-theirs".into(),
        name: "theirs".into(),
        version: 1,
        frozen: false,
        source: None,
        created_at: Utc::now(),
    };
    store.create_dataset(&ds).unwrap();

    let rubric = Rubric {
        id: new_id(),
        project_id: "proj-theirs".into(),
        name: "theirs".into(),
        dimensions: Vec::new(),
        threshold: 0.7,
        version: 1,
        supersedes: None,
        created_at: Utc::now(),
    };
    store.create_rubric(&rubric).unwrap();

    for uri in [
        format!("/v1/benchmarks/{}", bench.id),
        format!("/v1/benchmarks/{}/runs", bench.id),
        format!("/v1/datasets/{}", ds.id),
        format!("/v1/datasets/{}/items", ds.id),
        format!("/v1/rubrics/{}", rubric.id),
    ] {
        let (status, body) = get(&app, &uri, &mine).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{uri} must be 404 for another project's id — a 403 confirms the id exists"
        );
        assert_eq!(
            body["error"]["code"], "not_found",
            "{uri} must answer with the not_found envelope, not a bespoke refusal"
        );
    }

    // And an id that genuinely does not exist is indistinguishable from the above.
    let (status, _) = get(&app, &format!("/v1/benchmarks/{}", new_id()), &mine).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The queue's row had no tenant at all before M17. Enqueue stamps the benchmark's project, so a
/// scoped read of `GET /v1/jobs` returns one project's work and never the other's payloads.
#[tokio::test]
async fn the_job_queue_is_read_under_a_scope() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, "proj-a");
    make_key(&store, "proj-b");

    let mk = |pid: &str| Benchmark {
        id: new_id(),
        project_id: pid.into(),
        name: "b".into(),
        rubric: "r".into(),
        judge_model: "haiku".into(),
        target: json!([{ "provider": "anthropic", "model": "haiku" }]),
        dataset_ref: None,
        rubric_id: None,
        dataset: Vec::new(),
        baseline_score: None,
        created_at: Utc::now(),
    };
    let (ba, bb) = (mk("proj-a"), mk("proj-b"));
    store.create_benchmark(&ba).unwrap();
    store.create_benchmark(&bb).unwrap();

    let ja = crate::jobs_enqueue::enqueue(
        &state,
        Some("proj-a"),
        JobKind::BenchRun,
        json!({ "benchmark_id": ba.id }),
    )
    .await
    .ok()
    .expect("enqueue");
    let jb = crate::jobs_enqueue::enqueue(
        &state,
        Some("proj-b"),
        JobKind::BenchRun,
        json!({ "benchmark_id": bb.id }),
    )
    .await
    .ok()
    .expect("enqueue");

    let a_ids: Vec<String> = store
        .list_jobs(lighttrack_store::Scope::Project("proj-a"), None, 100)
        .unwrap()
        .into_iter()
        .map(|j| j.id)
        .collect();
    assert!(a_ids.contains(&ja.id), "a project sees its own queued work");
    assert!(
        !a_ids.contains(&jb.id),
        "and never another project's — the payload carries its benchmark id and its inputs"
    );

    let all: Vec<String> = store
        .list_jobs(lighttrack_store::Scope::Operator, None, 100)
        .unwrap()
        .into_iter()
        .map(|j| j.id)
        .collect();
    assert!(
        all.contains(&ja.id) && all.contains(&jb.id),
        "the operator still sees the whole queue, which is what makes it operable"
    );
}
