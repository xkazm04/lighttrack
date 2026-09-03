//! Typed verdict identity end to end: the `/v1/scores` filters, and rubric versioning as a
//! copy-with-changes rather than an edit.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use crate::redact::Redactor;
use crate::tests_ingest::{make_key, setup};

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn score(rubric: &str, rubric_id: Option<&str>, kind: &str) -> Value {
    json!({
        "project_id": "proj-a",
        "rubric": rubric,
        "rubric_id": rubric_id,
        "kind": kind,
        "value": 0.8, "max": 1.0,
        "scored_by": "judge",
    })
}

/// A benchmark case and an ad-hoc verdict are different measurements, and averaging them together
/// is the mistake the filters exist to prevent. Both predicates must actually narrow.
#[tokio::test]
async fn scores_can_be_narrowed_by_rubric_and_by_kind() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    for s in [
        score("quality", Some("rub-a"), "rubric"),
        score("quality", Some("rub-a"), "rubric"),
        score("bench:q#case1", Some("rub-a"), "bench_case"),
        score("ad-hoc criteria", None, "freeform"),
        score("other", Some("rub-b"), "rubric"),
    ] {
        let (status, _) = send(&app, "POST", "/v1/scores", &key, Some(s)).await;
        assert_eq!(status, StatusCode::OK);
    }

    let count = |body: &Value| body.as_array().expect("array").len();

    let (status, all) = send(&app, "GET", "/v1/scores", &key, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(count(&all), 5, "unfiltered listing is unchanged");

    let (_, by_rubric) = send(&app, "GET", "/v1/scores?rubric_id=rub-a", &key, None).await;
    assert_eq!(count(&by_rubric), 3);

    let (_, by_kind) = send(&app, "GET", "/v1/scores?kind=bench_case", &key, None).await;
    assert_eq!(count(&by_kind), 1, "an unfiltered default would return 5");
    assert_eq!(by_kind[0]["kind"], "bench_case");

    let (_, both) = send(
        &app,
        "GET",
        "/v1/scores?rubric_id=rub-a&kind=rubric",
        &key,
        None,
    )
    .await;
    assert_eq!(count(&both), 2, "AND, not OR");
}

/// "No bench cases" and "you spelled the kind wrong" must not look identical — a typo that answers
/// with an empty page reads as an authoritative absence.
#[tokio::test]
async fn an_unknown_kind_is_a_bad_request_not_an_empty_page() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (status, body) = send(&app, "GET", "/v1/scores?kind=benchcase", &key, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.to_string().contains("bench_case"),
        "the message names the vocabulary: {body}"
    );
}

/// A new version is a **new row with a new id**, linked to the old one — never a mutation. The
/// superseded rubric has to stay readable, because the verdicts that cite it still cite it.
#[tokio::test]
async fn a_rubric_version_supersedes_rather_than_edits() {
    let (state, store) = setup(Redactor::off());
    let _key = make_key(&store, "proj-a");
    let app = crate::build_router(state);
    let admin = "admin-secret";

    let (status, v1) = send(
        &app,
        "POST",
        "/v1/projects/proj-a/rubrics",
        admin,
        Some(json!({
            "name": "quality",
            "threshold": 0.7,
            "dimensions": [{ "key": "correct", "description": "right?", "weight": 1.0 }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v1_id = v1["id"].as_str().expect("id").to_string();
    assert!(
        v1.get("version").is_none(),
        "generation 1 is omitted from the wire (absent means 1): {v1}"
    );

    let (status, v2) = send(
        &app,
        "POST",
        &format!("/v1/rubrics/{v1_id}/versions"),
        admin,
        Some(json!({ "threshold": 0.85 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v2["version"], 2);
    assert_eq!(v2["supersedes"], v1_id.as_str(), "the chain is walkable");
    assert_ne!(v2["id"], v1_id.as_str(), "a new row, not an edit");
    assert_eq!(v2["threshold"], 0.85);
    assert_eq!(
        v2["dimensions"], v1["dimensions"],
        "an omitted half is carried forward, not blanked"
    );
    assert_eq!(v2["name"], "quality");

    // The superseded rubric is untouched and still readable — this is the whole point.
    let (status, still) = send(&app, "GET", &format!("/v1/rubrics/{v1_id}"), admin, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(still["threshold"], 0.7);

    // Versioning an unknown rubric is a 404, not a silently-minted orphan.
    let (status, _) = send(
        &app,
        "POST",
        "/v1/rubrics/nope/versions",
        admin,
        Some(json!({ "threshold": 0.9 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
