//! OpenTelemetry **GenAI semantic conventions** → [`LlmEvent`].
//!
//! The conventions have churned (and three widely-deployed instrumentations predate or extend them),
//! so every field reads a *list* of accepted attribute names, newest first. The exact set we accept:
//!
//! | `LlmEvent` field | attributes, in precedence order |
//! |---|---|
//! | `provider`   | `gen_ai.provider.name`, `gen_ai.system`, `llm.provider`, `llm.system`, `ai.model.provider` |
//! | `model`      | `gen_ai.request.model`, `gen_ai.response.model`, `llm.model_name`, `llm.request.model`, `ai.model.id` |
//! | `operation`  | `gen_ai.operation.name`, `llm.operation.name`, `openinference.span.kind` |
//! | `usage.input` | `gen_ai.usage.input_tokens`, `gen_ai.usage.prompt_tokens` *(legacy)*, `llm.token_count.prompt`, `llm.usage.prompt_tokens`, `ai.usage.promptTokens` |
//! | `usage.output` | `gen_ai.usage.output_tokens`, `gen_ai.usage.completion_tokens` *(legacy)*, `llm.token_count.completion`, `llm.usage.completion_tokens`, `ai.usage.completionTokens` |
//! | `usage.cached_input` | `gen_ai.usage.cached_input_tokens`, `gen_ai.usage.cache_read_input_tokens`, `llm.token_count.prompt_details.cache_read` |
//! | `usage.reasoning` | `gen_ai.usage.reasoning_tokens`, `gen_ai.usage.output_reasoning_tokens`, `llm.token_count.completion_details.reasoning` |
//! | `cost_usd`   | `gen_ai.usage.cost`, `gen_ai.usage.total_cost`, `llm.usage.total_cost` (non-standard; when absent the price book prices the call) |
//! | `input`      | `gen_ai.input.messages`, `gen_ai.prompt`, `llm.prompts`, `input.value` |
//! | `output`     | `gen_ai.output.messages`, `gen_ai.completion`, `llm.completions`, `output.value` |
//! | `name`       | `lighttrack.name`, else the span name |
//! | `project_id` | `lighttrack.project_id` (resource or span), else the `?project=` query param |
//!
//! `gen_ai.usage.total_tokens` alone is **not** mapped: a total cannot be split into input/output
//! without corrupting cost math, so such a span is priced from whatever split it does carry.
//!
//! Identity: `trace_id`/`span_id`/`parent_span_id` come straight from the span, and the event id is
//! `"<traceId>-<spanId>"` — deterministic, so a retried OTLP export replays into the existing
//! duplicate-acknowledgement path instead of double-counting.

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

use lighttrack_core::{new_id, LlmEvent, Operation, Provider, Status, TokenUsage};

use super::proto::FlatSpan;

/// A span we refuse, with a stable machine-readable code (see `otlp` module docs).
pub(crate) struct SpanReject {
    pub code: &'static str,
    pub reason: String,
}

impl SpanReject {
    fn new(code: &'static str, reason: impl Into<String>) -> Self {
        Self { code, reason: reason.into() }
    }
}

const PROVIDER_KEYS: &[&str] =
    &["gen_ai.provider.name", "gen_ai.system", "llm.provider", "llm.system", "ai.model.provider"];
const MODEL_KEYS: &[&str] = &[
    "gen_ai.request.model",
    "gen_ai.response.model",
    "llm.model_name",
    "llm.request.model",
    "ai.model.id",
];
const OPERATION_KEYS: &[&str] =
    &["gen_ai.operation.name", "llm.operation.name", "openinference.span.kind"];
const INPUT_TOKEN_KEYS: &[&str] = &[
    "gen_ai.usage.input_tokens",
    "gen_ai.usage.prompt_tokens",
    "llm.token_count.prompt",
    "llm.usage.prompt_tokens",
    "ai.usage.promptTokens",
];
const OUTPUT_TOKEN_KEYS: &[&str] = &[
    "gen_ai.usage.output_tokens",
    "gen_ai.usage.completion_tokens",
    "llm.token_count.completion",
    "llm.usage.completion_tokens",
    "ai.usage.completionTokens",
];
const CACHED_TOKEN_KEYS: &[&str] = &[
    "gen_ai.usage.cached_input_tokens",
    "gen_ai.usage.cache_read_input_tokens",
    "llm.token_count.prompt_details.cache_read",
];
const REASONING_TOKEN_KEYS: &[&str] = &[
    "gen_ai.usage.reasoning_tokens",
    "gen_ai.usage.output_reasoning_tokens",
    "llm.token_count.completion_details.reasoning",
];
const COST_KEYS: &[&str] = &["gen_ai.usage.cost", "gen_ai.usage.total_cost", "llm.usage.total_cost"];
const PROMPT_KEYS: &[&str] =
    &["gen_ai.input.messages", "gen_ai.prompt", "llm.prompts", "input.value"];
const COMPLETION_KEYS: &[&str] =
    &["gen_ai.output.messages", "gen_ai.completion", "llm.completions", "output.value"];

/// Map one flattened span onto an `LlmEvent`, or reject it with a code.
pub(crate) fn map_span(fs: &FlatSpan<'_>, default_project: Option<&str>) -> Result<LlmEvent, SpanReject> {
    if !is_genai(fs) {
        return Err(SpanReject::new(
            "not_genai",
            "span carries no GenAI attributes (expected at least one `gen_ai.*` / `llm.*` key)",
        ));
    }
    let model = fs
        .first(MODEL_KEYS)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            SpanReject::new(
                "bad_request",
                "GenAI span has no model attribute (`gen_ai.request.model` / `gen_ai.response.model`)",
            )
        })?
        .to_string();

    let start = fs.span.start_time_unix_nano.as_ref().and_then(|n| n.as_i128()).and_then(from_nanos);
    let end = fs.span.end_time_unix_nano.as_ref().and_then(|n| n.as_i128()).and_then(from_nanos);
    let latency_ms = match (start, end) {
        (Some(s), Some(e)) if e >= s => Some((e - s).num_milliseconds().max(0) as u64),
        _ => None,
    };

    let (status, error) = status_of(fs);
    let project_id = fs
        .first(&["lighttrack.project_id"])
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| default_project.map(str::to_string))
        .unwrap_or_default();

    Ok(LlmEvent {
        id: event_id(fs),
        project_id,
        trace_id: nonempty(fs.span.trace_id.as_deref()),
        span_id: nonempty(fs.span.span_id.as_deref()),
        parent_span_id: nonempty(fs.span.parent_span_id.as_deref()),
        ts: start.unwrap_or_else(Utc::now),
        provider: provider_of(fs),
        model,
        name: fs
            .first(&["lighttrack.name"])
            .and_then(|v| v.as_str())
            .or(fs.span.name.as_deref())
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
        operation: operation_of(fs),
        usage: TokenUsage {
            input: fs.first(INPUT_TOKEN_KEYS).and_then(|v| v.as_u64()).unwrap_or(0),
            output: fs.first(OUTPUT_TOKEN_KEYS).and_then(|v| v.as_u64()).unwrap_or(0),
            cached_input: fs.first(CACHED_TOKEN_KEYS).and_then(|v| v.as_u64()),
            reasoning: fs.first(REASONING_TOKEN_KEYS).and_then(|v| v.as_u64()),
        },
        // Only an explicitly exported cost is honored; otherwise `prepare_event` prices from the book.
        cost_usd: fs.first(COST_KEYS).and_then(|v| v.as_f64()).filter(|c| c.is_finite() && *c >= 0.0),
        latency_ms,
        status,
        error,
        // Payloads go in the redactable fields, never metadata — the persistence policy and the PII
        // scrubber only reach `input`/`output`.
        input: fs.first(PROMPT_KEYS).map(|v| v.to_json()),
        output: fs.first(COMPLETION_KEYS).map(|v| v.to_json()),
        tags: Vec::new(),
        source: Some("otlp".to_string()),
        metadata: metadata_of(fs),
    })
}

/// A span is GenAI-shaped if it carries any conventional attribute namespace we understand. Anything
/// else in the export (HTTP servers, DB clients, …) is not an LLM call and is refused, not stored.
fn is_genai(fs: &FlatSpan<'_>) -> bool {
    fs.attrs
        .keys()
        .any(|k| k.starts_with("gen_ai.") || k.starts_with("llm.") || k.starts_with("ai.model."))
}

/// Deterministic id so a re-exported span replays instead of duplicating. Falls back to a fresh id
/// only when the exporter omitted both ids (which no conforming SDK does).
fn event_id(fs: &FlatSpan<'_>) -> String {
    match (nonempty(fs.span.trace_id.as_deref()), nonempty(fs.span.span_id.as_deref())) {
        (Some(t), Some(s)) => format!("{t}-{s}"),
        (None, Some(s)) => s,
        _ => new_id(),
    }
}

fn nonempty(s: Option<&str>) -> Option<String> {
    s.map(str::trim).filter(|v| !v.is_empty()).map(str::to_lowercase)
}

fn from_nanos(nanos: i128) -> Option<DateTime<Utc>> {
    let secs = i64::try_from(nanos.div_euclid(1_000_000_000)).ok()?;
    let rem = nanos.rem_euclid(1_000_000_000) as u32;
    DateTime::from_timestamp(secs, rem)
}

/// `gen_ai.system` values are namespaced (`az.ai.openai`, `gcp.gemini`, `vertex_ai`, …); match on the
/// family rather than the exact string. An unmodeled provider stays `Unknown` — accepted, unpriced,
/// with the raw string preserved in metadata.
fn provider_of(fs: &FlatSpan<'_>) -> Provider {
    let raw = match fs.first(PROVIDER_KEYS).and_then(|v| v.as_str()) {
        Some(s) => s.to_ascii_lowercase(),
        None => return Provider::Unknown,
    };
    // `az.ai.openai` (Azure-hosted OpenAI) is still OpenAI models on OpenAI price-book keys.
    if raw.contains("openai") {
        return Provider::OpenAi;
    }
    if raw.contains("anthropic") {
        return Provider::Anthropic;
    }
    if raw.contains("gemini") || raw.contains("vertex") || raw.contains("google") {
        return Provider::Google;
    }
    Provider::Unknown
}

fn operation_of(fs: &FlatSpan<'_>) -> Operation {
    let raw = match fs.first(OPERATION_KEYS).and_then(|v| v.as_str()) {
        Some(s) => s.to_ascii_lowercase(),
        None => return Operation::default(),
    };
    if raw.contains("embed") {
        return Operation::Embedding;
    }
    if raw.contains("text_completion") || raw == "completion" {
        return Operation::Completion;
    }
    if raw.contains("chat") || raw.contains("generate_content") || raw == "llm" {
        return Operation::Chat;
    }
    Operation::Other
}

/// Span status → event status. An error whose type or message reads as a timeout is classified
/// `timeout` (LightTrack's error-spike and reliability views distinguish the two).
fn status_of(fs: &FlatSpan<'_>) -> (Status, Option<String>) {
    let st = match &fs.span.status {
        Some(s) if s.is_error() => s,
        _ => return (Status::Success, None),
    };
    let err_type = fs.first(&["error.type", "exception.type"]).and_then(|v| v.as_str());
    let message = st
        .message
        .as_deref()
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .or_else(|| exception_message(fs))
        .or_else(|| err_type.map(str::to_string));
    let haystack =
        format!("{} {}", err_type.unwrap_or_default(), message.clone().unwrap_or_default())
            .to_ascii_lowercase();
    let status = if haystack.contains("timeout") || haystack.contains("timed out") {
        Status::Timeout
    } else {
        Status::Error
    };
    (status, message)
}

/// The `exception.message` of a recorded exception event, the usual carrier of the failure detail.
fn exception_message(fs: &FlatSpan<'_>) -> Option<String> {
    fs.span
        .events
        .iter()
        .find(|e| e.name.as_deref() == Some("exception"))
        .and_then(|e| {
            e.attributes
                .iter()
                .find(|kv| kv.key == "exception.message")
                .and_then(|kv| kv.value.as_ref())
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
}

/// Provenance that has no `LlmEvent` column, kept under `metadata.otel` so every store backend
/// carries it unchanged: the raw system string, the *other* model attribute, the instrumentation
/// scope, and the response id / finish reasons.
fn metadata_of(fs: &FlatSpan<'_>) -> Value {
    let mut otel = Map::new();
    if let Some(fr) = fs.attr("gen_ai.response.finish_reasons") {
        otel.insert("finish_reasons".to_string(), fr.to_json());
    }
    let mut put = |k: &str, v: Option<&str>| {
        if let Some(v) = v.filter(|s| !s.is_empty()) {
            otel.insert(k.to_string(), Value::String(v.to_string()));
        }
    };
    put("system", fs.first(PROVIDER_KEYS).and_then(|v| v.as_str()));
    put("request_model", fs.attr("gen_ai.request.model").and_then(|v| v.as_str()));
    put("response_model", fs.attr("gen_ai.response.model").and_then(|v| v.as_str()));
    put("response_id", fs.attr("gen_ai.response.id").and_then(|v| v.as_str()));
    put("operation", fs.first(OPERATION_KEYS).and_then(|v| v.as_str()));
    put("scope", fs.scope);
    put("service_name", fs.attr("service.name").and_then(|v| v.as_str()));
    put("span_name", fs.span.name.as_deref());
    Value::Object([("otel".to_string(), Value::Object(otel))].into_iter().collect())
}
