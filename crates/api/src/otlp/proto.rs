//! The subset of the OTLP/HTTP **JSON** trace envelope LightTrack consumes.
//!
//! Hand-rolled serde structs rather than `opentelemetry-proto`: we read ~12 fields of one message,
//! and pulling a prost/tonic-generated crate (plus its protoc/build-script surface) into an axum
//! service to deserialize a JSON body would be a large dependency for no capability we use. The cost
//! of hand-rolling is that we must be liberal in what we accept — which we are, deliberately:
//!
//! - Field names in **both** `camelCase` (what the OTel Collector and every SDK exporter emit) and
//!   `snake_case` (what a raw protojson encoder may emit).
//! - 64-bit integers as JSON **strings** (canonical proto3 JSON) *or* numbers (what several SDKs
//!   actually write) — see [`Num`].
//! - Enum values as numbers *or* their proto enum names (`2` or `"STATUS_CODE_ERROR"`).
//! - `traceId`/`spanId` as **hex strings**, which is what the OTLP/JSON spec mandates for ID fields
//!   (it deviates from protobuf's base64 `bytes` encoding). Base64-encoded IDs are NOT decoded; they
//!   pass through verbatim, so a non-conforming encoder yields an odd-looking but stable id.
//!
//! Everything we do not read is ignored (no `deny_unknown_fields`), so a newer OTLP revision keeps
//! working.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportTraceServiceRequest {
    #[serde(default, alias = "resource_spans")]
    pub resource_spans: Vec<ResourceSpans>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResourceSpans {
    #[serde(default)]
    pub resource: Option<Resource>,
    #[serde(default, alias = "scope_spans", alias = "instrumentationLibrarySpans")]
    pub scope_spans: Vec<ScopeSpans>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Resource {
    #[serde(default)]
    pub attributes: Vec<KeyValue>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScopeSpans {
    #[serde(default)]
    pub scope: Option<Scope>,
    #[serde(default)]
    pub spans: Vec<Span>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Scope {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Span {
    #[serde(default, alias = "trace_id")]
    pub trace_id: Option<String>,
    #[serde(default, alias = "span_id")]
    pub span_id: Option<String>,
    #[serde(default, alias = "parent_span_id")]
    pub parent_span_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, alias = "start_time_unix_nano")]
    pub start_time_unix_nano: Option<Num>,
    #[serde(default, alias = "end_time_unix_nano")]
    pub end_time_unix_nano: Option<Num>,
    #[serde(default)]
    pub attributes: Vec<KeyValue>,
    #[serde(default)]
    pub events: Vec<SpanEvent>,
    #[serde(default)]
    pub status: Option<SpanStatus>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct SpanEvent {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub attributes: Vec<KeyValue>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct SpanStatus {
    #[serde(default)]
    pub code: Option<EnumValue>,
    #[serde(default)]
    pub message: Option<String>,
}

impl SpanStatus {
    /// `true` for `STATUS_CODE_ERROR` (proto value `2`). `UNSET`/`OK` are both non-errors.
    pub fn is_error(&self) -> bool {
        match &self.code {
            Some(EnumValue::Num(n)) => *n == 2,
            Some(EnumValue::Name(s)) => s.eq_ignore_ascii_case("STATUS_CODE_ERROR"),
            None => false,
        }
    }
}

/// A proto enum in JSON: the numeric value or the enum name.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum EnumValue {
    Num(i64),
    Name(String),
}

/// A proto 64-bit integer in JSON. Canonical proto3 JSON encodes `int64`/`fixed64` as a **string**;
/// several SDK JSON exporters write a bare number instead. Accept both (and a float, which is what a
/// language without 64-bit ints produces).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum Num {
    Int(i64),
    Float(f64),
    Str(String),
}

impl Num {
    pub fn as_i128(&self) -> Option<i128> {
        match self {
            Num::Int(i) => Some(*i as i128),
            Num::Float(f) => Some(*f as i128),
            Num::Str(s) => s.trim().parse::<i128>().ok(),
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.as_i128().and_then(|v| u64::try_from(v).ok())
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Num::Int(i) => Some(*i as f64),
            Num::Float(f) => Some(*f),
            Num::Str(s) => s.trim().parse::<f64>().ok(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct KeyValue {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub value: Option<AnyValue>,
}

/// OTLP `AnyValue`: exactly one of these is set. Modeled as all-optional (rather than an untagged
/// enum) so an unknown/extra variant never fails the whole export.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AnyValue {
    #[serde(default, alias = "string_value")]
    pub string_value: Option<String>,
    #[serde(default, alias = "int_value")]
    pub int_value: Option<Num>,
    #[serde(default, alias = "double_value")]
    pub double_value: Option<f64>,
    #[serde(default, alias = "bool_value")]
    pub bool_value: Option<bool>,
    #[serde(default, alias = "array_value")]
    pub array_value: Option<ArrayValue>,
    #[serde(default, alias = "kvlist_value")]
    pub kvlist_value: Option<KvList>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ArrayValue {
    #[serde(default)]
    pub values: Vec<AnyValue>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct KvList {
    #[serde(default)]
    pub values: Vec<KeyValue>,
}

impl AnyValue {
    pub fn as_str(&self) -> Option<&str> {
        self.string_value.as_deref()
    }

    /// Numeric reading of the value, tolerating a stringified number (`"42"`) — GenAI token counts
    /// are frequently exported as strings.
    pub fn as_u64(&self) -> Option<u64> {
        if let Some(n) = &self.int_value {
            return n.as_u64();
        }
        if let Some(d) = self.double_value {
            return (d >= 0.0).then_some(d as u64);
        }
        self.string_value.as_deref().and_then(|s| s.trim().parse::<u64>().ok())
    }

    pub fn as_f64(&self) -> Option<f64> {
        if let Some(d) = self.double_value {
            return Some(d);
        }
        if let Some(n) = &self.int_value {
            return n.as_f64();
        }
        self.string_value.as_deref().and_then(|s| s.trim().parse::<f64>().ok())
    }

    /// Plain-JSON projection, used for payload attributes (prompts/completions). A string that is
    /// itself JSON is parsed, so `gen_ai.prompt = "[{\"role\":\"user\"}]"` lands as a real array —
    /// which is what makes the PII scrubber able to walk it.
    pub fn to_json(&self) -> Value {
        if let Some(s) = &self.string_value {
            let t = s.trim();
            if t.starts_with('{') || t.starts_with('[') {
                if let Ok(v) = serde_json::from_str::<Value>(t) {
                    return v;
                }
            }
            return Value::String(s.clone());
        }
        if let Some(b) = self.bool_value {
            return Value::Bool(b);
        }
        if let Some(d) = self.double_value {
            return serde_json::Number::from_f64(d).map(Value::Number).unwrap_or(Value::Null);
        }
        if let Some(n) = &self.int_value {
            return n.as_i128().and_then(|v| i64::try_from(v).ok()).map(Value::from).unwrap_or(Value::Null);
        }
        if let Some(a) = &self.array_value {
            return Value::Array(a.values.iter().map(AnyValue::to_json).collect());
        }
        if let Some(kv) = &self.kvlist_value {
            let m: Map<String, Value> = kv
                .values
                .iter()
                .map(|k| (k.key.clone(), k.value.as_ref().map(AnyValue::to_json).unwrap_or(Value::Null)))
                .collect();
            return Value::Object(m);
        }
        Value::Null
    }
}

/// One span lifted out of the `resourceSpans → scopeSpans → spans` nesting, with its resource and
/// scope context attached. Attribute precedence is span > resource (a span-level
/// `lighttrack.project_id` overrides the resource-level default).
pub(crate) struct FlatSpan<'a> {
    pub span: &'a Span,
    pub scope: Option<&'a str>,
    pub attrs: HashMap<&'a str, &'a AnyValue>,
}

impl<'a> FlatSpan<'a> {
    pub fn attr(&self, key: &str) -> Option<&'a AnyValue> {
        self.attrs.get(key).copied()
    }

    /// First present attribute among `keys`, in order — the alias-tolerance primitive.
    pub fn first(&self, keys: &[&str]) -> Option<&'a AnyValue> {
        keys.iter().find_map(|k| self.attr(k))
    }
}

/// Flatten the export envelope into a positional span list. The index of each entry is the span's
/// position in this request and is what the response's per-span outcomes refer to.
pub(crate) fn flatten(req: &ExportTraceServiceRequest) -> Vec<FlatSpan<'_>> {
    let mut out = Vec::new();
    for rs in &req.resource_spans {
        let mut base: HashMap<&str, &AnyValue> = HashMap::new();
        if let Some(r) = &rs.resource {
            collect(&r.attributes, &mut base);
        }
        for ss in &rs.scope_spans {
            let scope = ss.scope.as_ref().and_then(|s| s.name.as_deref());
            for span in &ss.spans {
                let mut attrs = base.clone();
                collect(&span.attributes, &mut attrs);
                out.push(FlatSpan { span, scope, attrs });
            }
        }
    }
    out
}

fn collect<'a>(kvs: &'a [KeyValue], into: &mut HashMap<&'a str, &'a AnyValue>) {
    for kv in kvs {
        if let Some(v) = &kv.value {
            into.insert(kv.key.as_str(), v);
        }
    }
}
