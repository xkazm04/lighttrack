//! End-to-end over the wired router: an unknown difficulty tier is refused on every surface that
//! accepts one, and the refusal costs the whole request rather than the one grade.
//!
//! These reproduce a live incident (2026-09-07). A model×effort benchmark graded 8 of its 18 cases
//! `expert`. The API answered 200 and stored all 8 ungraded; the per-tier analysis then reported a
//! bucket with nothing in it, and only a division by zero in an analysis script made anyone look.
//! The listing filter had been refusing the same spelling on the query string the whole time.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use crate::redact::Redactor;
use crate::tests_ingest::setup;

const ADMIN: &str = "admin-secret";

async fn send(app: &axum::Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {ADMIN}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn ok(app: &axum::Router, method: &str, uri: &str, body: Value) -> Value {
    let (st, v) = send(app, method, uri, body).await;
    assert_eq!(st, StatusCode::OK, "{method} {uri}: {v}");
    v
}

fn app() -> axum::Router {
    let (state, _store) = setup(Redactor::off());
    crate::build_router(state)
}

/// Every refusal says the same three things, whichever surface produced it.
fn assert_names_the_ladder(body: &Value) {
    let msg = body["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("a refusal carries the envelope's message: {body}"));
    assert_eq!(body["error"]["code"], "bad_request", "{body}");
    for rung in ["easy", "medium", "hard"] {
        assert!(msg.contains(rung), "the refusal names {rung}: {msg}");
    }
}

/// The incident itself. A fourth rung on part of an inline dataset refuses the POST, and leaves no
/// half-graded benchmark behind for a report to read as complete.
#[tokio::test]
async fn a_fourth_tier_in_an_inline_dataset_refuses_the_whole_benchmark() {
    let app = app();
    let mut cases = Vec::new();
    for i in 0..18 {
        // The live shape: most of the corpus on the ladder, a minority on a rung nobody defined.
        let tier = if i % 2 == 0 && i < 16 {
            "expert"
        } else {
            "hard"
        };
        cases.push(json!({ "input": format!("case {i}"), "difficulty": tier }));
    }
    let (status, body) = send(
        &app,
        "POST",
        "/v1/projects/p1/benchmarks",
        json!({ "name": "effort-matrix", "rubric": "is it right", "dataset": cases }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_names_the_ladder(&body);
    let msg = body["error"]["message"].as_str().expect("message");
    assert!(
        msg.contains("expert"),
        "the refused spelling is quoted: {msg}"
    );
    assert!(
        msg.contains("dataset case 0"),
        "the offending case is named: {msg}"
    );

    // The load-bearing half: nothing was created. A benchmark whose 8 `expert` cases had been
    // quietly downgraded to ungraded would look, to every later reader, like a fully graded corpus.
    let benches = ok(&app, "GET", "/v1/projects/p1/benchmarks", json!({})).await;
    assert!(
        benches.as_array().expect("array").is_empty(),
        "the refused request must not have written a partially-graded benchmark: {benches}"
    );
}

/// The other direction: a dataset entirely on the ladder is created, and each grade survives the
/// round trip. Without this the fix above could be "refuse everything" and still pass.
#[tokio::test]
async fn an_inline_dataset_on_the_ladder_is_created_and_keeps_every_grade() {
    let app = app();
    let created = ok(
        &app,
        "POST",
        "/v1/projects/p1/benchmarks",
        json!({
            "name": "graded", "rubric": "is it right",
            "dataset": [
                { "input": "1+1", "difficulty": "easy" },
                { "input": "integrate it", "difficulty": "HARD" },
                { "input": "unexamined" },
                { "input": "explicitly ungraded", "difficulty": Value::Null },
            ],
        }),
    )
    .await;
    let ds = created["dataset"].as_array().expect("dataset");
    assert_eq!(ds.len(), 4);
    assert_eq!(ds[0]["difficulty"], "easy");
    // A shell spelling is normalised to the one the corpus stores, exactly as the listing filter
    // and `lt datasets add` already normalise it.
    assert_eq!(ds[1]["difficulty"], "hard");
    // Absent and null are the same UNGRADED, and neither becomes `medium`.
    assert_eq!(ds[2].get("difficulty"), None);
    assert_eq!(ds[3].get("difficulty"), None);
}

/// The other write surface, over the same closed vocabulary: one row per shape a caller can send.
///
/// The point of the table is that the last three waves each shipped a test that exercised one shape
/// of input. A number, an object and an empty string are all "a tier was stated" and all used to be
/// 200-and-forget.
#[tokio::test]
async fn every_stated_tier_on_a_dataset_item_is_accepted_or_refused_on_purpose() {
    let app = app();
    let ds = ok(
        &app,
        "POST",
        "/v1/projects/p1/datasets",
        json!({ "name": "tiers" }),
    )
    .await;
    let id = ds["id"].as_str().expect("id").to_string();

    // (body, the tier the corpus should end up holding) — every one of these is a 200.
    let accepted: [(Value, Value); 6] = [
        (json!({ "input": "a", "difficulty": "easy" }), json!("easy")),
        (
            json!({ "input": "b", "difficulty": "medium" }),
            json!("medium"),
        ),
        (json!({ "input": "c", "difficulty": "hard" }), json!("hard")),
        (json!({ "input": "d", "difficulty": "HARD" }), json!("hard")),
        (json!({ "input": "e" }), Value::Null),
        (
            json!({ "input": "f", "difficulty": Value::Null }),
            Value::Null,
        ),
    ];
    for (body, want) in accepted {
        let item = ok(&app, "POST", &format!("/v1/datasets/{id}/items"), body).await;
        assert_eq!(item["difficulty"], want, "echoed back on create");
    }

    // Every one of these is a 400, and none of them appends a case.
    for bad in [
        json!("expert"),
        json!("hardd"),
        json!(""),
        json!("   "),
        json!(3),
        json!({ "tier": "hard" }),
        json!(["hard"]),
    ] {
        let (status, body) = send(
            &app,
            "POST",
            &format!("/v1/datasets/{id}/items"),
            json!({ "input": "refused", "difficulty": bad }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "difficulty {bad}: {body}");
        assert_names_the_ladder(&body);
    }

    let items = ok(&app, "GET", &format!("/v1/datasets/{id}/items"), json!({})).await;
    let items = items.as_array().expect("array");
    assert_eq!(
        items.len(),
        6,
        "a refused grade must not leave a case behind: {items:?}"
    );
    assert!(items.iter().all(|i| i["input"] != "refused"), "{items:?}");
    // And nothing landed in the middle rung by accident — `None` is ungraded, never medium.
    let medium = ok(
        &app,
        "GET",
        &format!("/v1/datasets/{id}/items?difficulty=medium"),
        json!({}),
    )
    .await;
    assert_eq!(medium.as_array().expect("array").len(), 1, "{medium}");
}

/// The two surfaces answer the same typo with the same sentence. They used to answer it with a 400
/// and a 200 respectively, which is the entire defect.
#[tokio::test]
async fn the_filter_and_the_write_refuse_an_unknown_tier_the_same_way() {
    let app = app();
    let ds = ok(
        &app,
        "POST",
        "/v1/projects/p1/datasets",
        json!({ "name": "symmetry" }),
    )
    .await;
    let id = ds["id"].as_str().expect("id").to_string();

    let (read_status, read_body) = send(
        &app,
        "GET",
        &format!("/v1/datasets/{id}/items?difficulty=expert"),
        json!({}),
    )
    .await;
    let (write_status, write_body) = send(
        &app,
        "POST",
        &format!("/v1/datasets/{id}/items"),
        json!({ "input": "i", "difficulty": "expert" }),
    )
    .await;
    assert_eq!(read_status, StatusCode::BAD_REQUEST, "{read_body}");
    assert_eq!(write_status, StatusCode::BAD_REQUEST, "{write_body}");
    assert_eq!(
        read_body["error"]["message"], write_body["error"]["message"],
        "one vocabulary, one refusal"
    );
}
