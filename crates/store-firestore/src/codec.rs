//! Firestore typed-value codec (REST `Value`/`Document` <-> plain JSON) + field accessors and the
//! shared ts/enum helpers. Domain modules build a plain `serde_json::Map` of fields and read decoded
//! maps back, so they look like the SQL row mappers.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Map, Value};

use lighttrack_store::{Result, StoreError};

pub(crate) type Fields = Map<String, Value>;

pub(crate) fn other(msg: impl Into<String>) -> StoreError {
    StoreError::Other(msg.into())
}

pub(crate) fn missing(field: &str) -> StoreError {
    other(format!("firestore: missing field `{field}`"))
}

pub(crate) fn fmt_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub(crate) fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)
        .map_err(|e| other(format!("bad ts {s:?}: {e}")))?
        .with_timezone(&Utc))
}

pub(crate) fn enum_to_str<T: Serialize>(v: &T) -> Result<String> {
    serde_json::to_value(v)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| other("enum did not serialize to a string"))
}

pub(crate) fn parse_enum<T: DeserializeOwned + Default>(s: &str) -> T {
    serde_json::from_value(Value::String(s.to_string())).unwrap_or_default()
}

// --- typed value <-> plain JSON ---------------------------------------------

/// Plain JSON value -> Firestore typed value.
pub(crate) fn encode_value(v: &Value) -> Value {
    match v {
        Value::Null => json!({ "nullValue": null }),
        Value::Bool(b) => json!({ "booleanValue": b }),
        Value::String(s) => json!({ "stringValue": s }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                // Firestore integers are JSON strings.
                json!({ "integerValue": i.to_string() })
            } else {
                json!({ "doubleValue": n.as_f64().unwrap_or(0.0) })
            }
        }
        Value::Array(a) => {
            json!({ "arrayValue": { "values": a.iter().map(encode_value).collect::<Vec<_>>() } })
        }
        Value::Object(o) => {
            let f: Map<String, Value> =
                o.iter().map(|(k, v)| (k.clone(), encode_value(v))).collect();
            json!({ "mapValue": { "fields": f } })
        }
    }
}

/// Firestore typed value -> plain JSON value.
pub(crate) fn decode_value(t: &Value) -> Value {
    if let Some(s) = t.get("stringValue") {
        return s.clone();
    }
    if let Some(i) = t.get("integerValue") {
        if let Some(s) = i.as_str() {
            if let Ok(n) = s.parse::<i64>() {
                return json!(n);
            }
        }
        return i.clone();
    }
    if let Some(d) = t.get("doubleValue") {
        return d.clone();
    }
    if let Some(b) = t.get("booleanValue") {
        return b.clone();
    }
    if t.get("nullValue").is_some() {
        return Value::Null;
    }
    if let Some(ts) = t.get("timestampValue") {
        return ts.clone();
    }
    if let Some(arr) = t.get("arrayValue") {
        let vals = arr
            .get("values")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(decode_value).collect())
            .unwrap_or_default();
        return Value::Array(vals);
    }
    if let Some(m) = t.get("mapValue") {
        let mut o = Map::new();
        if let Some(f) = m.get("fields").and_then(Value::as_object) {
            for (k, v) in f {
                o.insert(k.clone(), decode_value(v));
            }
        }
        return Value::Object(o);
    }
    Value::Null
}

/// Encode a plain field map into a Firestore `fields` object.
pub(crate) fn encode_fields(fields: &Fields) -> Value {
    let m: Map<String, Value> = fields
        .iter()
        .map(|(k, v)| (k.clone(), encode_value(v)))
        .collect();
    Value::Object(m)
}

/// Decode a Firestore document's `fields` into a plain map.
pub(crate) fn decode_doc(doc: &Value) -> Fields {
    let mut out = Fields::new();
    if let Some(f) = doc.get("fields").and_then(Value::as_object) {
        for (k, v) in f {
            out.insert(k.clone(), decode_value(v));
        }
    }
    out
}

// --- field accessors over a decoded map -------------------------------------

pub(crate) fn fstr(m: &Fields, k: &str) -> Option<String> {
    m.get(k).and_then(Value::as_str).map(str::to_string)
}
pub(crate) fn freq(m: &Fields, k: &str) -> Result<String> {
    fstr(m, k).ok_or_else(|| missing(k))
}
pub(crate) fn fi64(m: &Fields, k: &str) -> Option<i64> {
    m.get(k).and_then(Value::as_i64)
}
pub(crate) fn ff64(m: &Fields, k: &str) -> Option<f64> {
    m.get(k).and_then(Value::as_f64)
}
pub(crate) fn fbool(m: &Fields, k: &str) -> bool {
    fi64(m, k).map(|v| v != 0).unwrap_or(false)
}
/// Parse a string field that holds a JSON blob into a `Value` (null when absent).
pub(crate) fn fjson(m: &Fields, k: &str) -> Result<Value> {
    match fstr(m, k) {
        Some(s) => Ok(serde_json::from_str(&s)?),
        None => Ok(Value::Null),
    }
}

/// Parse an optional JSON-string field into `Option<Value>` (None when the field is absent/null).
pub(crate) fn fopt_json(m: &Fields, k: &str) -> Result<Option<Value>> {
    match fstr(m, k) {
        Some(s) => Ok(Some(serde_json::from_str(&s)?)),
        None => Ok(None),
    }
}

/// Serialize an `Option<Value>` to an optional JSON string for storage.
pub(crate) fn opt_json_str(v: &Option<Value>) -> Result<Option<String>> {
    match v {
        Some(x) => Ok(Some(serde_json::to_string(x)?)),
        None => Ok(None),
    }
}

/// Serialize a `Value` to a JSON string, or None when it is JSON null.
pub(crate) fn json_or_null_str(v: &Value) -> Result<Option<String>> {
    if v.is_null() {
        Ok(None)
    } else {
        Ok(Some(serde_json::to_string(v)?))
    }
}
