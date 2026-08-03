//! OTLP ingest tests, driven through the **wired router** (`crate::build_router`) so the whole stack
//! under test is the real one: auth → span mapping → the native batch handler → validation →
//! redaction → price book → admission → store.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use lighttrack_core::{new_id, LimitAction, LimitMetric, LimitRule, LimitWindow, Status};
use lighttrack_store::Store;

use crate::redact::Redactor;
use crate::tests_ingest::{make_key, setup};

/// A realistic OTLP/HTTP JSON export as an OTel SDK emits it: one resource, one instrumentation
/// scope, and a canonical `gen_ai.*` chat span (current semconv attribute names, int64 fields as
/// JSON strings, hex ids) plus a child span using the **legacy** `prompt_tokens`/`completion_tokens`
/// aliases, plus a non-GenAI HTTP span that must be refused.
fn fixture() -> Value {
    json!({
      "resourceSpans": [{
        "resource": { "attributes": [
          { "key": "service.name", "value": { "stringValue": "checkout-api" } }
        ]},
        "scopeSpans": [{
          "scope": { "name": "opentelemetry.instrumentation.anthropic" },
          "spans": [
            {
              "traceId": "5b8efff798038103d269b633813fc60c",
              "spanId": "eee19b7ec3c1b174",
              "name": "chat claude-haiku-4-5",
              "kind": 3,
              "startTimeUnixNano": "1785578400000000000",
              "endTimeUnixNano": "1785578401500000000",
              "attributes": [
                { "key": "gen_ai.system", "value": { "stringValue": "anthropic" } },
                { "key": "gen_ai.operation.name", "value": { "stringValue": "chat" } },
                { "key": "gen_ai.request.model", "value": { "stringValue": "claude-haiku-4-5" } },
                { "key": "gen_ai.response.model", "value": { "stringValue": "claude-haiku-4-5-20260101" } },
                { "key": "gen_ai.usage.input_tokens", "value": { "intValue": "1000000" } },
                { "key": "gen_ai.usage.output_tokens", "value": { "intValue": "1000000" } },
                { "key": "gen_ai.response.id", "value": { "stringValue": "msg_01ABC" } },
                { "key": "gen_ai.prompt", "value": { "stringValue": "[{\"role\":\"user\",\"content\":\"mail jane@example.com\"}]" } }
              ],
              "status": { "code": 1 }
            },
            {
              "traceId": "5b8efff798038103d269b633813fc60c",
              "spanId": "eee19b7ec3c1b175",
              "parentSpanId": "eee19b7ec3c1b174",
              "name": "chat claude-haiku-4-5",
              "startTimeUnixNano": 1785578402000000000i64,
              "endTimeUnixNano": 1785578402250000000i64,
              "attributes": [
                { "key": "gen_ai.system", "value": { "stringValue": "anthropic" } },
                { "key": "gen_ai.request.model", "value": { "stringValue": "claude-haiku-4-5" } },
                { "key": "gen_ai.usage.prompt_tokens", "value": { "intValue": "10" } },
                { "key": "gen_ai.usage.completion_tokens", "value": { "intValue": "5" } }
              ],
              "status": { "code": "STATUS_CODE_ERROR", "message": "upstream request timed out" }
            },
            {
              "traceId": "5b8efff798038103d269b633813fc60c",
              "spanId": "eee19b7ec3c1b176",
              "name": "GET /checkout",
              "attributes": [
                { "key": "http.request.method", "value": { "stringValue": "GET" } }
              ]
            }
          ]
        }]
      }]
    })
}

async fn export(app: &Router, token: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/traces")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    (status, v)
}

#[tokio::test]
async fn canonical_export_round_trips_to_events() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (status, body) = export(&app, &key, fixture()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["lighttrack"]["accepted"], 2, "{body}");
    assert_eq!(body["lighttrack"]["unmapped"], 1, "{body}");
    // OTLP partial success: the non-GenAI span is reported as rejected, in the standard field.
    assert_eq!(body["partialSuccess"]["rejectedSpans"], 1, "{body}");

    let rows = store.list_events(Some("proj-a"), 10).unwrap();
    assert_eq!(rows.len(), 2, "only the GenAI spans became events");

    let root = rows.iter().find(|e| e.span_id.as_deref() == Some("eee19b7ec3c1b174")).unwrap();
    assert_eq!(root.trace_id.as_deref(), Some("5b8efff798038103d269b633813fc60c"));
    assert!(root.parent_span_id.is_none());
    assert_eq!(root.id, "5b8efff798038103d269b633813fc60c-eee19b7ec3c1b174", "deterministic id");
    assert_eq!(root.provider.as_str(), "anthropic");
    assert_eq!(root.model, "claude-haiku-4-5", "request model drives pricing");
    assert_eq!(root.metadata["otel"]["response_model"], "claude-haiku-4-5-20260101");
    assert_eq!(root.metadata["otel"]["service_name"], "checkout-api");
    assert_eq!(root.usage.input, 1_000_000);
    assert_eq!(root.usage.output, 1_000_000);
    assert_eq!(root.latency_ms, Some(1500));
    assert_eq!(root.status, Status::Success);
    assert_eq!(root.ts.timestamp(), 1_785_578_400, "startTimeUnixNano became the event timestamp");
    assert_eq!(root.source.as_deref(), Some("otlp"));
    // Priced from the DB book by the SHARED pipeline: 1M in @ $1 + 1M out @ $5 = $6.
    assert!((root.cost_usd.unwrap() - 6.0).abs() < 1e-9, "{:?}", root.cost_usd);
    assert_eq!(root.metadata["cost_source"], "book");
    // The prompt attribute landed in the redactable payload field, parsed out of its JSON string.
    assert_eq!(root.input.as_ref().unwrap()[0]["role"], "user");

    // The child span: legacy token aliases, numeric (not string) nanos, parent link, error status
    // classified as a timeout from the status message.
    let child = rows.iter().find(|e| e.span_id.as_deref() == Some("eee19b7ec3c1b175")).unwrap();
    assert_eq!(child.parent_span_id.as_deref(), Some("eee19b7ec3c1b174"));
    assert_eq!(child.usage.input, 10, "legacy gen_ai.usage.prompt_tokens accepted");
    assert_eq!(child.usage.output, 5, "legacy gen_ai.usage.completion_tokens accepted");
    assert_eq!(child.latency_ms, Some(250));
    assert_eq!(child.status, Status::Timeout);
    assert_eq!(child.error.as_deref(), Some("upstream request timed out"));
}

#[tokio::test]
async fn non_genai_and_modelless_spans_are_rejected_with_codes() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (status, body) = export(
        &app,
        &key,
        json!({ "resourceSpans": [{ "scopeSpans": [{ "spans": [
            { "traceId": "aa", "spanId": "b1", "name": "GET /health",
              "attributes": [{ "key": "http.route", "value": { "stringValue": "/health" } }] },
            { "traceId": "aa", "spanId": "b2", "name": "chat",
              "attributes": [{ "key": "gen_ai.system", "value": { "stringValue": "openai" } }] }
        ]}]}]}),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "a fully-rejected export is still OTLP partial success");
    let results = body["lighttrack"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "{body}");
    assert_eq!(results[0]["status"], "unmapped");
    assert_eq!(results[0]["code"], "not_genai", "{body}");
    assert_eq!(results[0]["index"], 0);
    assert_eq!(results[1]["code"], "bad_request", "GenAI-shaped but no model: {body}");
    assert_eq!(body["partialSuccess"]["rejectedSpans"], 2, "{body}");
    assert!(store.list_events(Some("proj-a"), 10).unwrap().is_empty(), "nothing was stored");
}

#[tokio::test]
async fn otlp_ingest_respects_limits_and_redaction() {
    // The guarantee that matters: OTLP is not a side door. An enforcing cap rejects the span and the
    // PII scrubber runs over the mapped payload, exactly as on POST /v1/events.
    let (state, store) = setup(Redactor::all());
    let key = make_key(&store, "proj-a");
    store
        .create_limit_rule(&LimitRule {
            id: new_id(),
            project_id: "proj-a".into(),
            metric: LimitMetric::Calls,
            window: LimitWindow::Hour,
            threshold: 1.0, // the first span reaches the cap
            action: LimitAction::Block,
            enabled: true,
            warn_at: None,
            scope: None,
        })
        .unwrap();
    let app = crate::build_router(state);

    let (status, body) = export(&app, &key, fixture()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["lighttrack"]["accepted"], 0, "an enforcing cap must reject OTLP spans: {body}");
    assert_eq!(body["lighttrack"]["rejected"], 2, "{body}");
    let results = body["lighttrack"]["results"].as_array().unwrap();
    assert!(
        results.iter().any(|r| r["code"] == "rate_limited"),
        "the batch taxonomy's code is surfaced per span: {body}"
    );
    assert!(store.list_events(Some("proj-a"), 10).unwrap().is_empty(), "nothing stored over cap");

    // Same export without the cap: the prompt is scrubbed before it is persisted.
    let (state2, store2) = setup(Redactor::all());
    let key2 = make_key(&store2, "proj-a");
    let app2 = crate::build_router(state2);
    let (s2, b2) = export(&app2, &key2, fixture()).await;
    assert_eq!(s2, StatusCode::OK, "{b2}");
    let stored = serde_json::to_string(&store2.list_events(Some("proj-a"), 10).unwrap()).unwrap();
    assert!(!stored.contains("jane@example.com"), "raw PII persisted from an OTLP span: {stored}");
    assert!(stored.contains("<EMAIL>"), "redaction marker missing: {stored}");
}

#[tokio::test]
async fn a_replayed_export_does_not_double_count() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (s1, _) = export(&app, &key, fixture()).await;
    assert_eq!(s1, StatusCode::OK);
    // The exporter retries the same batch after a response timeout: span ids are deterministic, so
    // the native replay path acknowledges both without writing them again.
    let (s2, b2) = export(&app, &key, fixture()).await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(b2["lighttrack"]["accepted"], 2, "a replay is acknowledged, not refused: {b2}");
    assert_eq!(store.list_events(Some("proj-a"), 10).unwrap().len(), 2, "no double-count");
}

#[tokio::test]
async fn a_project_key_scopes_otlp_spans_to_its_own_project() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    // The span claims another project both by attribute and by query param; the key wins.
    let mut body = fixture();
    body["resourceSpans"][0]["resource"]["attributes"].as_array_mut().unwrap().push(
        json!({ "key": "lighttrack.project_id", "value": { "stringValue": "proj-b" } }),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/v1/traces?project=proj-b")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert!(store.list_events(Some("proj-b"), 10).unwrap().is_empty(), "tenant boundary crossed");
    assert_eq!(store.list_events(Some("proj-a"), 10).unwrap().len(), 2);
}

#[tokio::test]
async fn an_unauthorized_export_is_401_even_when_nothing_maps() {
    let (state, store) = setup(Redactor::off());
    let _ = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (status, _) = export(
        &app,
        "lt_not-a-real-key",
        json!({ "resourceSpans": [{ "scopeSpans": [{ "spans": [
            { "traceId": "aa", "spanId": "b1", "name": "GET /health" }
        ]}]}]}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_export_resolves_the_project_from_attributes_and_query() {
    let (state, _store) = setup(Redactor::off());
    let app = crate::build_router(state);

    // No project anywhere: the native "project_id is required" invalid outcome, per span.
    let (status, body) = export(&app, "admin-secret", fixture()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["lighttrack"]["invalid"], 2, "{body}");
    let results = body["lighttrack"]["results"].as_array().unwrap();
    assert!(results.iter().any(|r| r["code"] == "bad_request"), "{body}");
}

#[tokio::test]
async fn empty_export_is_a_clean_success() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (status, body) = export(&app, &key, json!({ "resourceSpans": [] })).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("partialSuccess").is_none(), "clean export omits partialSuccess: {body}");
    assert_eq!(body["lighttrack"]["accepted"], 0);
}
