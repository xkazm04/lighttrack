//! OTLP ingest tests, driven through the **wired router** (`crate::build_router`) so the whole stack
//! under test is the real one: auth → span mapping → the native batch handler → validation →
//! redaction → price book → admission → store.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use lighttrack_core::{
    new_id, LimitAction, LimitMetric, LimitRule, LimitWindow, Status, Threshold,
};
use lighttrack_store::Store;

use crate::redact::Redactor;
use crate::tests_ingest::{make_key, setup};

/// A fixture span's `startTimeUnixNano`, as an OTel SDK would emit it **now**.
///
/// It used to be the calendar constant `1785578400000000000` (2026-08-01 10:00 UTC). Ingest refuses
/// a `ts` more than `LIGHTTRACK_MAX_TS_SKEW_PAST_SECS` (7 days) behind the server, so on 2026-08-08
/// every OTLP test in this file began failing on a tree nobody had touched, and `cargo test
/// --workspace` — a BLOCKING check — went red and stayed red. A fixture pinned to a wall-clock
/// instant is a test whose verdict is a function of the calendar rather than of the commit.
///
/// So the fixture is anchored to the run: one minute ago, comfortably inside the skew window in
/// both directions, and unaffected by the passage of time.
///
/// Resolved once per process, so every fixture and every assertion in this file share one instant —
/// a per-call `Utc::now()` would make "the start time became the event timestamp" unassertable.
fn base_nanos() -> i64 {
    static BASE: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *BASE.get_or_init(|| {
        (chrono::Utc::now() - chrono::Duration::seconds(60))
            .timestamp_nanos_opt()
            .expect("a timestamp near now is representable in nanoseconds")
    })
}

/// `base_nanos()` plus a fixed offset in milliseconds, as a JSON string — the form an OTel exporter
/// uses for int64 fields. Offsets, not absolutes, so the spans keep their exact relative durations
/// (which is what the latency assertions actually pin).
fn at_ms(offset_ms: i64) -> String {
    (base_nanos() + offset_ms * 1_000_000).to_string()
}

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
              "startTimeUnixNano": at_ms(0),
              "endTimeUnixNano": at_ms(1500),
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
              "startTimeUnixNano": at_ms(2000),
              "endTimeUnixNano": at_ms(2250),
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
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
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

    let root = rows
        .iter()
        .find(|e| e.span_id.as_deref() == Some("eee19b7ec3c1b174"))
        .unwrap();
    assert_eq!(
        root.trace_id.as_deref(),
        Some("5b8efff798038103d269b633813fc60c")
    );
    assert!(root.parent_span_id.is_none());
    assert_eq!(
        root.id, "5b8efff798038103d269b633813fc60c-eee19b7ec3c1b174",
        "deterministic id"
    );
    assert_eq!(root.provider.as_str(), "anthropic");
    assert_eq!(
        root.model, "claude-haiku-4-5",
        "request model drives pricing"
    );
    assert_eq!(
        root.metadata["otel"]["response_model"],
        "claude-haiku-4-5-20260101"
    );
    assert_eq!(root.metadata["otel"]["service_name"], "checkout-api");
    assert_eq!(root.usage.input, 1_000_000);
    assert_eq!(root.usage.output, 1_000_000);
    assert_eq!(root.latency_ms, Some(1500));
    assert_eq!(root.status, Status::Success);
    assert_eq!(
        root.ts.timestamp_nanos_opt(),
        at_ms(0).parse::<i64>().ok(),
        "startTimeUnixNano became the event timestamp"
    );
    assert_eq!(root.source.as_deref(), Some("otlp"));
    // Priced from the DB book by the SHARED pipeline: 1M in @ $1 + 1M out @ $5 = $6.
    assert!(
        (root.cost_usd.unwrap() - 6.0).abs() < 1e-9,
        "{:?}",
        root.cost_usd
    );
    assert_eq!(root.metadata["cost_source"], "book");
    // The prompt attribute landed in the redactable payload field, parsed out of its JSON string.
    assert_eq!(root.input.as_ref().unwrap()[0]["role"], "user");

    // The child span: legacy token aliases, numeric (not string) nanos, parent link, error status
    // classified as a timeout from the status message.
    let child = rows
        .iter()
        .find(|e| e.span_id.as_deref() == Some("eee19b7ec3c1b175"))
        .unwrap();
    assert_eq!(child.parent_span_id.as_deref(), Some("eee19b7ec3c1b174"));
    assert_eq!(
        child.usage.input, 10,
        "legacy gen_ai.usage.prompt_tokens accepted"
    );
    assert_eq!(
        child.usage.output, 5,
        "legacy gen_ai.usage.completion_tokens accepted"
    );
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

    assert_eq!(
        status,
        StatusCode::OK,
        "a fully-rejected export is still OTLP partial success"
    );
    let results = body["lighttrack"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "{body}");
    assert_eq!(results[0]["status"], "unmapped");
    assert_eq!(results[0]["code"], "not_genai", "{body}");
    assert_eq!(results[0]["index"], 0);
    assert_eq!(
        results[1]["code"], "bad_request",
        "GenAI-shaped but no model: {body}"
    );
    assert_eq!(body["partialSuccess"]["rejectedSpans"], 2, "{body}");
    assert!(
        store.list_events(Some("proj-a"), 10).unwrap().is_empty(),
        "nothing was stored"
    );
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
            threshold: Threshold::Fixed(1.0), // the first span reaches the cap
            action: LimitAction::Block,
            enabled: true,
            warn_at: None,
            scope: None,
            escalation: None,
            escalated_until: None,
            origin: None,
            expires_at: None,
        })
        .unwrap();
    let app = crate::build_router(state);

    let (status, body) = export(&app, &key, fixture()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["lighttrack"]["accepted"], 0,
        "an enforcing cap must reject OTLP spans: {body}"
    );
    assert_eq!(body["lighttrack"]["rejected"], 2, "{body}");
    let results = body["lighttrack"]["results"].as_array().unwrap();
    assert!(
        results.iter().any(|r| r["code"] == "rate_limited"),
        "the batch taxonomy's code is surfaced per span: {body}"
    );
    assert!(
        store.list_events(Some("proj-a"), 10).unwrap().is_empty(),
        "nothing stored over cap"
    );

    // Same export without the cap: the prompt is scrubbed before it is persisted.
    let (state2, store2) = setup(Redactor::all());
    let key2 = make_key(&store2, "proj-a");
    let app2 = crate::build_router(state2);
    let (s2, b2) = export(&app2, &key2, fixture()).await;
    assert_eq!(s2, StatusCode::OK, "{b2}");
    let stored = serde_json::to_string(&store2.list_events(Some("proj-a"), 10).unwrap()).unwrap();
    assert!(
        !stored.contains("jane@example.com"),
        "raw PII persisted from an OTLP span: {stored}"
    );
    assert!(
        stored.contains("<EMAIL>"),
        "redaction marker missing: {stored}"
    );
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
    assert_eq!(
        b2["lighttrack"]["accepted"], 2,
        "a replay is acknowledged, not refused: {b2}"
    );
    assert_eq!(
        store.list_events(Some("proj-a"), 10).unwrap().len(),
        2,
        "no double-count"
    );
}

#[tokio::test]
async fn a_project_key_scopes_otlp_spans_to_its_own_project() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    // The span claims another project both by attribute and by query param; the key wins.
    let mut body = fixture();
    body["resourceSpans"][0]["resource"]["attributes"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "key": "lighttrack.project_id", "value": { "stringValue": "proj-b" } }));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/traces?project=proj-b")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert!(
        store.list_events(Some("proj-b"), 10).unwrap().is_empty(),
        "tenant boundary crossed"
    );
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
    assert!(
        body.get("partialSuccess").is_none(),
        "clean export omits partialSuccess: {body}"
    );
    assert_eq!(body["lighttrack"]["accepted"], 0);
}

/// A realistic nested export: an HTTP-handler root with no `gen_ai.*` attributes, a tool span under
/// it (also non-GenAI), and the LLM calls hanging off those. Only the LLM spans become events — but
/// the trace must keep the shape it had in the exporter, one connected tree, not one root per span.
fn nested_fixture() -> Value {
    let span = |id: &str, parent: Option<&str>, name: &str, genai: bool| {
        let mut s = json!({
            "traceId": "9f2c4d6e8a0b1c3d5e7f9a1b2c3d4e5f",
            "spanId": id,
            "name": name,
            "startTimeUnixNano": at_ms(0),
            "endTimeUnixNano": at_ms(1000),
            "attributes": if genai {
                json!([
                    { "key": "gen_ai.system", "value": { "stringValue": "anthropic" } },
                    { "key": "gen_ai.request.model", "value": { "stringValue": "claude-haiku-4-5" } },
                    { "key": "gen_ai.usage.input_tokens", "value": { "intValue": "10" } },
                    { "key": "gen_ai.usage.output_tokens", "value": { "intValue": "5" } }
                ])
            } else {
                json!([{ "key": "http.request.method", "value": { "stringValue": "POST" } }])
            }
        });
        if let Some(p) = parent {
            s["parentSpanId"] = json!(p);
        }
        s
    };
    json!({
      "resourceSpans": [{
        "resource": { "attributes": [
          { "key": "service.name", "value": { "stringValue": "agent-api" } }
        ]},
        "scopeSpans": [{
          "scope": { "name": "opentelemetry.instrumentation.anthropic" },
          "spans": [
            // HTTP handler (dropped) -> plan LLM call.
            span("aaaa000000000001", None, "POST /agent", false),
            span("bbbb000000000002", Some("aaaa000000000001"), "chat plan", true),
            // plan -> tool span (dropped) -> the LLM call the tool made.
            span("cccc000000000003", Some("bbbb000000000002"), "tool.search", false),
            span("dddd000000000004", Some("cccc000000000003"), "chat summarize", true),
          ]
        }]
      }]
    })
}

#[tokio::test]
async fn an_otel_trace_keeps_its_shape_when_non_genai_spans_are_dropped() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (status, body) = export(&app, &key, nested_fixture()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["lighttrack"]["accepted"], 2,
        "only the LLM spans are stored: {body}"
    );
    assert_eq!(
        body["lighttrack"]["unmapped"], 2,
        "the HTTP + tool spans are refused: {body}"
    );

    // No phantom LLM events were invented for the dropped spans.
    let rows = store.list_events(Some("proj-a"), 10).unwrap();
    assert_eq!(rows.len(), 2, "the event table holds LLM calls only");

    // The LLM call under the dropped HTTP handler has no GenAI ancestor: a genuine root.
    let plan = rows
        .iter()
        .find(|e| e.span_id.as_deref() == Some("bbbb000000000002"))
        .unwrap();
    assert!(
        plan.parent_span_id.is_none() || plan.parent_span_id.as_deref() == Some("aaaa000000000001")
    );

    // The LLM call under the dropped TOOL span is reparented onto the plan call — the link survives.
    let summarize = rows
        .iter()
        .find(|e| e.span_id.as_deref() == Some("dddd000000000004"))
        .unwrap();
    assert_eq!(
        summarize.parent_span_id.as_deref(),
        Some("bbbb000000000002"),
        "parent chain must survive the dropped tool span"
    );
    assert_eq!(
        summarize.metadata["otel"]["otlp_parent_span_id"], "cccc000000000003",
        "the exporter's own parent is recorded, so the synthesized link is visible as synthesized"
    );

    // And the trace reads as ONE connected tree rather than N roots.
    let trace = store
        .get_trace(
            Some("proj-a"),
            "9f2c4d6e8a0b1c3d5e7f9a1b2c3d4e5f",
            lighttrack_store::MAX_TRACE_SPANS,
        )
        .unwrap()
        .expect("trace");
    assert_eq!(
        trace.spans.len(),
        1,
        "one root, not one per dropped parent: {:?}",
        trace.spans
    );
    assert_eq!(
        trace.spans[0].children.len(),
        1,
        "the summarize call nests under the plan call"
    );
}

/// A namespaced `gen_ai.system` is kept **as sent** (M8) instead of being coerced into one of three
/// literals, and still prices — from its family's rows, since nothing declares an `az.ai.openai`
/// price. The id is what limit scopes and rollups group on, so losing it lost the operator's own
/// vocabulary; collapsing it into `anthropic` lost the fact that this was Azure.
#[tokio::test]
async fn a_namespaced_vendor_id_is_kept_and_still_prices() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-az");
    let app = crate::build_router(state);

    let body = json!({
      "resourceSpans": [{
        "scopeSpans": [{
          "spans": [{
            "traceId": "5b8efff798038103d269b633813fc60d",
            "spanId": "eee19b7ec3c1b175",
            "name": "chat gpt-4o-mini",
            "startTimeUnixNano": at_ms(0),
            "endTimeUnixNano": at_ms(10),
            "attributes": [
              { "key": "gen_ai.system", "value": { "stringValue": "az.ai.anthropic" } },
              { "key": "gen_ai.request.model", "value": { "stringValue": "claude-haiku-4-5" } },
              { "key": "gen_ai.usage.input_tokens", "value": { "intValue": "1000000" } }
            ]
          }]
        }]
      }]
    });
    let (status, resp) = export(&app, &key, body).await;
    assert_eq!(status, StatusCode::OK, "{resp}");

    let rows = store.list_events(Some("proj-az"), 10).unwrap();
    assert_eq!(rows.len(), 1, "{resp}");
    assert_eq!(
        rows[0].provider.as_str(),
        "az.ai.anthropic",
        "the vendor id the exporter sent is what we store"
    );
    assert_eq!(
        rows[0].cost_usd,
        Some(1.0),
        "an Anthropic-family id prices from the Anthropic rows"
    );
}

/// One logical trace that spans an OTel-instrumented service and an SDK-instrumented service. The
/// SDK sends the W3C trace id in upper case (nothing normalized it before); OTLP lower-cases. Both
/// doors now canonicalize identically, so this renders as one trace instead of two.
#[tokio::test]
async fn a_mixed_otel_and_sdk_trace_is_one_trace() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (status, body) = export(&app, &key, nested_fixture()).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The SDK-side service reports the same trace, upper-cased, parented on the OTel plan span.
    let (status, v) = crate::tests_ingest::ingest(
        &app,
        &key,
        json!({
            "provider": "anthropic", "model": "claude-haiku-4-5",
            "usage": { "input": 10, "output": 5 }, "cost_usd": 0.001,
            "trace_id": "9F2C4D6E8A0B1C3D5E7F9A1B2C3D4E5F",
            "span_id": "EEEE000000000005",
            "parent_span_id": "BBBB000000000002"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");

    let trace = store
        .get_trace(
            Some("proj-a"),
            "9f2c4d6e8a0b1c3d5e7f9a1b2c3d4e5f",
            lighttrack_store::MAX_TRACE_SPANS,
        )
        .unwrap()
        .expect("the OTel and SDK halves are one trace");
    assert_eq!(
        trace.totals.spans, 3,
        "SDK span joined the OTel trace: {:?}",
        trace.spans
    );
    assert_eq!(trace.spans.len(), 1, "still one connected tree");
    // The SDK span parented onto the OTel plan span despite the case difference.
    let plan = &trace.spans[0];
    assert_eq!(plan.event.span_id.as_deref(), Some("bbbb000000000002"));
    assert_eq!(
        plan.children.len(),
        2,
        "both the OTel and the SDK child hang off the plan call"
    );
}

/// A caller's own opaque trace id keeps its case — folding it would merge distinct traces and mangle
/// an id the operator reads back.
#[tokio::test]
async fn a_non_w3c_trace_id_is_not_case_folded() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    for tid in ["Order-7", "order-7"] {
        let (status, v) = crate::tests_ingest::ingest(
            &app,
            &key,
            json!({
                "provider": "anthropic", "model": "claude-haiku-4-5",
                "usage": { "input": 1, "output": 1 }, "cost_usd": 0.0,
                "trace_id": tid, "span_id": format!("s-{tid}")
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
    }
    let upper = store
        .get_trace(Some("proj-a"), "Order-7", lighttrack_store::MAX_TRACE_SPANS)
        .unwrap();
    assert_eq!(
        upper.expect("kept verbatim").totals.spans,
        1,
        "distinct opaque ids stay distinct"
    );
}
