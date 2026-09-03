//! The M26 loop, end to end over HTTP: see the gap → add the price → the numbers become honest.
//!
//! The assertion that matters is the *sequence*, not the three routes separately. Each one is
//! individually plausible while the loop is broken — a ledger nobody can act on, a fill that leaves
//! the ledger unchanged, a history that never gains a second row.

use axum::body::{to_bytes, Body};
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use crate::redact::Redactor;
use crate::tests_ingest::{ingest, make_key, setup};

const ADMIN: &str = "admin-secret";

async fn send(
    app: &axum::Router,
    method: &str,
    token: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(match body {
            Some(v) => Body::from(v.to_string()),
            None => Body::empty(),
        })
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("response");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn ev(id: &str, model: &str) -> Value {
    json!({
        "id": id,
        "provider": "acme",
        "model": model,
        "usage": { "input": 1000000, "output": 0 },
    })
}

#[tokio::test]
async fn the_unpriced_ledger_names_the_gap_and_a_fill_closes_it() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-u");
    let app = crate::build_router(state);

    // Traffic on a model the test book has never heard of: stored, counted, and costed at NULL.
    for id in ["u-1", "u-2", "u-3"] {
        let (status, body) = ingest(&app, &key, ev(id, "zoo-1")).await;
        assert_eq!(status, StatusCode::OK, "{id}");
        assert!(
            body["cost_usd"].is_null(),
            "an unpriceable call must store no cost, not a zero"
        );
    }

    // 1. See the gap. A project key reads its own — this is not admin-only, because the operator
    //    who needs to know their cost figure is a floor is the one who owns the project.
    let (status, _, body) = send(&app, "GET", &key, "/v1/costs/unpriced", None).await;
    assert_eq!(status, StatusCode::OK);
    let models = body["models"].as_array().expect("models");
    assert_eq!(models.len(), 1, "one unpriced key");
    assert_eq!(models[0]["provider"], "acme");
    assert_eq!(models[0]["model"], "zoo-1");
    assert_eq!(models[0]["calls"], 3);
    assert_eq!(models[0]["input_tokens"], 3_000_000);
    assert_eq!(body["unpriced_calls"], 3);
    assert!(
        body["notes"].as_str().unwrap_or_default().contains("FLOOR"),
        "the ledger says outright that the cost numbers are a floor"
    );
    assert!(
        body["price_book"]["stale"].is_boolean(),
        "the book's own freshness travels with the ledger"
    );

    // 2. Add the price, and ask for the history to be priced on the same call.
    let (status, _, body) = send(
        &app,
        "PUT",
        ADMIN,
        "/v1/prices/acme/zoo-1?fill_unpriced=1",
        Some(json!({
            "input_per_mtok": 2.0, "output_per_mtok": 4.0,
            "verified_at": "2026-08-01", "note": "vendor page"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["filled"], 3, "all three stored rows were priced");
    assert_eq!(body["remaining_unpriced"], 0);
    assert_eq!(body["note"], "vendor page");
    assert!(
        body["effective_from"].is_string(),
        "the row reports the point on the timeline it took"
    );

    // 3. The numbers are honest: the ledger is empty and the cost is no longer a floor.
    let (_, _, body) = send(&app, "GET", &key, "/v1/costs/unpriced", None).await;
    assert_eq!(body["unpriced_calls"], 0);
    assert!(body["models"].as_array().expect("models").is_empty());

    let (status, headers, body) = send(&app, "GET", &key, "/v1/costs", None).await;
    assert_eq!(status, StatusCode::OK);
    let row = &body.as_array().expect("rows")[0];
    assert_eq!(row["unpriced_calls"], 0);
    assert!(
        (row["cost_usd"].as_f64().expect("cost") - 6.0).abs() < 1e-9,
        "3 calls x 1M input @ $2/Mtok, filled: {row}"
    );
    assert!(
        headers.contains_key("x-price-book-stale"),
        "every cost read discloses how fresh the rates behind it are"
    );

    // A second fill is a no-op — the property that makes the flag safe to type twice.
    let (_, _, body) = send(
        &app,
        "PUT",
        ADMIN,
        "/v1/prices/acme/zoo-1?fill_unpriced=1",
        Some(json!({ "input_per_mtok": 2.0, "output_per_mtok": 4.0 })),
    )
    .await;
    assert_eq!(body["filled"], 0);
}

#[tokio::test]
async fn a_corrected_rate_appends_to_the_timeline_instead_of_erasing_it() {
    let (state, store) = setup(Redactor::off());
    let _key = make_key(&store, "proj-h");
    let app = crate::build_router(state);

    for (rate, from) in [(1.0, "2026-01-01"), (3.0, "2026-06-01")] {
        let (status, _, _) = send(
            &app,
            "PUT",
            ADMIN,
            "/v1/prices/acme/dated-1",
            Some(json!({
                "input_per_mtok": rate, "output_per_mtok": 0.0, "effective_from": from
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    let (status, _, body) = send(&app, "GET", ADMIN, "/v1/prices/history/acme/dated-1", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().expect("history");
    assert_eq!(rows.len(), 2, "the January rate survived the June one");
    assert_eq!(rows[0]["input_per_mtok"], 3.0, "newest first");

    // The current book carries exactly one row for the key — the rate in force.
    let (_, _, body) = send(&app, "GET", ADMIN, "/v1/prices", None).await;
    let current: Vec<&Value> = body
        .as_array()
        .expect("prices")
        .iter()
        .filter(|r| r["model"] == "dated-1")
        .collect();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0]["input_per_mtok"], 3.0);
}

/// A fill is a write over stored rows. A project key — even one that may read the ledger — must not
/// be able to start one.
#[tokio::test]
async fn only_an_admin_may_write_a_price_or_start_a_fill() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (status, _, _) = send(
        &app,
        "PUT",
        &key,
        "/v1/prices/acme/zoo-1?fill_unpriced=1",
        Some(json!({ "input_per_mtok": 1.0, "output_per_mtok": 1.0 })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// A mistyped date lands the rate at the wrong point on a timeline nobody is watching, so it is a
/// 400 rather than a silent "now".
#[tokio::test]
async fn a_malformed_effective_from_is_refused() {
    let (state, store) = setup(Redactor::off());
    let _key = make_key(&store, "proj-b");
    let app = crate::build_router(state);

    let (status, _, _) = send(
        &app,
        "PUT",
        ADMIN,
        "/v1/prices/acme/zoo-1",
        Some(json!({
            "input_per_mtok": 1.0, "output_per_mtok": 1.0, "effective_from": "soon"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
