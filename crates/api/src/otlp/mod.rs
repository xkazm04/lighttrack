//! OTLP/HTTP **JSON** trace ingest: `POST /v1/traces`.
//!
//! The industry's default instrumentation is OpenTelemetry, and its GenAI semantic conventions
//! already describe exactly what an `LlmEvent` is. This is the second front door of the ingestion
//! contract (`docs/ARCHITECTURE.md` §4): point any OTel SDK's OTLP/HTTP exporter at the LightTrack
//! API (`OTEL_EXPORTER_OTLP_ENDPOINT=http://host:8787`, which appends `/v1/traces`) and GenAI spans
//! become events. Non-goals in this slice: **no gRPC**, no metrics/logs signals, no proxy mode.
//!
//! ## It is not a second ingest path
//! Mapping is the *only* thing this module does. The mapped events are handed to
//! [`crate::events_batch::post_batch`] — the same handler `POST /v1/events/batch` uses — so auth,
//! project scoping, validation, the project's payload-persistence policy, PII redaction, price-book
//! costing, and single-critical-section limit admission are byte-for-byte the native behavior. There
//! is no way to reach the store from here that bypasses a cap or a redaction policy.
//!
//! ## Response contract
//! HTTP **200** with the OTLP `ExportTraceServiceResponse` shape, so a stock OTel exporter parses it:
//!
//! ```json
//! { "partialSuccess": { "rejectedSpans": 2, "errorMessage": "…" },
//!   "lighttrack": { "accepted": 3, "unmapped": 1, "rejected": 1, "invalid": 0, "results": [ … ] } }
//! ```
//!
//! `partialSuccess` is omitted entirely on a clean export (the spec's success shape). `rejectedSpans`
//! counts every span that was **not** stored. The additive `lighttrack` object — which OTLP consumers
//! ignore as an unknown field — carries LightTrack's own multi-status per-item detail, in the batch
//! endpoint's taxonomy so one client branch covers both doors:
//!
//! | `code` | meaning |
//! |---|---|
//! | `not_genai` | the span carries no GenAI attributes at all — not an LLM call (OTLP-only code) |
//! | `bad_request` | GenAI-shaped but unmappable or invalid (e.g. no model attribute) |
//! | `rate_limited` | an enforcing limit breach turned it away; not stored |
//! | `conflict` | that span id already exists with a different payload |
//! | `internal` | store failure on that item; siblings still committed |
//!
//! Nothing is ever silently dropped: every span in the request appears in `results` with an outcome.

mod linkage;
mod proto;
mod semconv;

#[cfg(test)]
mod tests;

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ApiError;
use crate::ingest_proximity::{Proximity, WithProximity};
use crate::state::AppState;

#[derive(Deserialize)]
pub(crate) struct OtlpParams {
    /// Project for spans that don't carry a `lighttrack.project_id` attribute. Ignored when the
    /// caller presents a project API key (that key forces its own project, exactly as on
    /// `POST /v1/events`).
    project: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct OtlpResponse {
    #[serde(rename = "partialSuccess", skip_serializing_if = "Option::is_none")]
    partial_success: Option<PartialSuccess>,
    lighttrack: Summary,
}

#[derive(Serialize)]
struct PartialSuccess {
    #[serde(rename = "rejectedSpans")]
    rejected_spans: u64,
    #[serde(rename = "errorMessage")]
    error_message: String,
}

#[derive(Serialize)]
struct Summary {
    accepted: usize,
    /// Spans that never became events (not GenAI, or GenAI-shaped but unmappable).
    unmapped: usize,
    /// Mapped events an enforcing limit breach turned away.
    rejected: usize,
    /// Mapped events refused by validation or a store constraint.
    invalid: usize,
    results: Vec<SpanOutcome>,
}

/// One span's outcome. `index` is the span's position in the flattened request (resourceSpans →
/// scopeSpans → spans, in order), so correlation is explicit rather than implied.
#[derive(Serialize)]
struct SpanOutcome {
    index: usize,
    #[serde(rename = "spanId", skip_serializing_if = "Option::is_none")]
    span_id: Option<String>,
    /// `accepted` | `rejected` | `invalid` | `unmapped`.
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// The stored event id (`"<traceId>-<spanId>"`) when the span was accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

pub(crate) async fn post_traces(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<OtlpParams>,
    Json(req): Json<proto::ExportTraceServiceRequest>,
) -> Result<WithProximity<OtlpResponse>, ApiError> {
    let spans = proto::flatten(&req);
    let mut outcomes: Vec<Option<SpanOutcome>> = (0..spans.len()).map(|_| None).collect();
    let mut events = Vec::new();
    let mut span_of_event: Vec<usize> = Vec::new();

    for (i, fs) in spans.iter().enumerate() {
        let span_id = fs.span.span_id.clone();
        match semconv::map_span(fs, q.project.as_deref()) {
            Ok(ev) => {
                span_of_event.push(i);
                events.push(ev);
            }
            Err(rej) => {
                outcomes[i] = Some(SpanOutcome {
                    index: i,
                    span_id,
                    status: "unmapped",
                    code: Some(rej.code.to_string()),
                    reason: Some(rej.reason),
                    id: None,
                })
            }
        }
    }

    // The dropped non-GenAI spans (HTTP handlers, tools, DB calls) were often the *parents* of these
    // LLM spans. Reparenting onto the nearest ancestor that did map keeps the trace one connected tree
    // instead of N roots — see `linkage`. Done before the write so the stored event carries the link.
    linkage::reparent_past_dropped_spans(&spans, &mut events, &span_of_event);

    // Nothing mappable: authenticate anyway (the batch handler would have, and an unauthorized
    // caller must not learn how its payload was classified), then report the export as fully rejected.
    if events.is_empty() {
        crate::guards::authenticate(&st, &headers).await?;
        let results: Vec<SpanOutcome> = outcomes.into_iter().flatten().collect();
        return Ok(WithProximity::new(finish(0, results), Proximity::default()));
    }

    // The one and only write path: the native batch handler. Its response is read back through JSON
    // rather than by destructuring a private type, which keeps this module additive.
    // The OTLP envelope is the exporter's shape, not ours, so the proximity signal cannot ride in
    // the body at all here — it is carried through unchanged from the batch door onto the response
    // headers, which is the one channel all three ingest doors share.
    let batch = crate::events_batch::post_batch(State(st), headers, Json(events)).await?;
    let body = serde_json::to_value(&batch.body)
        .map_err(|e| ApiError::internal(format!("batch response encode error: {e}")))?;

    let mut accepted = 0usize;
    for item in body
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let k = item.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        let Some(&i) = span_of_event.get(k) else {
            continue;
        };
        let status = item
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("invalid");
        let (status, code) = match status {
            "accepted" => {
                accepted += 1;
                ("accepted", None)
            }
            "rejected" => ("rejected", str_field(item, "code")),
            _ => ("invalid", str_field(item, "code")),
        };
        outcomes[i] = Some(SpanOutcome {
            index: i,
            span_id: spans[i].span.span_id.clone(),
            status,
            code,
            reason: str_field(item, "reason"),
            id: str_field(item, "id"),
        });
    }

    let results: Vec<SpanOutcome> = outcomes.into_iter().flatten().collect();
    Ok(WithProximity::new(
        finish(accepted, results),
        batch.proximity,
    ))
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Tally the per-span outcomes into the OTLP partial-success envelope + LightTrack summary.
fn finish(accepted: usize, results: Vec<SpanOutcome>) -> OtlpResponse {
    let count = |s: &str| results.iter().filter(|r| r.status == s).count();
    let (unmapped, rejected, invalid) = (count("unmapped"), count("rejected"), count("invalid"));
    let refused = unmapped + rejected + invalid;
    let partial_success = (refused > 0).then(|| PartialSuccess {
        rejected_spans: refused as u64,
        error_message: format!(
            "{refused} of {} span(s) not recorded ({unmapped} unmapped, {rejected} limit-rejected, \
             {invalid} invalid) — see the `lighttrack.results` field for per-span codes",
            results.len()
        ),
    });
    OtlpResponse {
        partial_success,
        lighttrack: Summary {
            accepted,
            unmapped,
            rejected,
            invalid,
            results,
        },
    }
}
