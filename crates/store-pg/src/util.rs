//! Shared helpers for the Postgres backend: timestamp formatting, enum (de)serialization, sqlx
//! error mapping, and JSON-column round-tripping.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use lighttrack_store::{Result, StoreError};

pub(crate) fn fmt_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub(crate) fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)
        .map_err(|e| StoreError::Other(format!("bad ts {s:?}: {e}")))?
        .with_timezone(&Utc))
}

pub(crate) fn enum_to_str<T: Serialize>(v: &T) -> Result<String> {
    serde_json::to_value(v)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| StoreError::Other("enum did not serialize to a string".into()))
}

pub(crate) fn parse_enum<T: DeserializeOwned + Default>(s: &str) -> T {
    serde_json::from_value(Value::String(s.to_string())).unwrap_or_default()
}

pub(crate) fn pgerr(e: sqlx::Error) -> StoreError {
    StoreError::Other(format!("postgres: {e}"))
}

pub(crate) fn json_or_null(v: &Value) -> Result<Option<String>> {
    if v.is_null() {
        Ok(None)
    } else {
        Ok(Some(serde_json::to_string(v)?))
    }
}

pub(crate) fn val_or_null(s: Option<String>) -> Result<Value> {
    match s {
        Some(x) => Ok(serde_json::from_str(&x)?),
        None => Ok(Value::Null),
    }
}
