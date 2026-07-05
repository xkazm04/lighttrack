//! Cross-backend codec helpers shared by every [`Store`](crate::Store) implementation.
//!
//! The on-the-wire encoding of timestamps, string-valued enums, and JSON columns is part of the
//! storage contract, not a per-backend detail: SQLite, Postgres, and Firestore all map the same Rust
//! types to the same strings. These helpers live here once so a new backend reuses them and an
//! existing one can't silently diverge. The fixed-width timestamp format in particular is a
//! documented invariant — see [`fmt_ts`].

use chrono::{DateTime, SecondsFormat, Utc};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::{Result, StoreError};

/// Fixed-width, UTC, nanosecond RFC3339 (e.g. `2026-05-31T00:07:14.110948400Z`). Fixed width =>
/// lexicographic ordering matches chronological ordering, so `ts` range filters / `ORDER BY` are
/// correct as plain string comparisons.
///
/// **This format is a cross-backend invariant.** Every store backend must encode timestamps through
/// this one function; tweaking it in a single backend would desync that backend's query ordering.
pub fn fmt_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// Parse a [`fmt_ts`]-encoded (or any RFC3339) timestamp back to UTC.
pub fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)
        .map_err(|e| StoreError::Other(format!("bad ts {s:?}: {e}")))?
        .with_timezone(&Utc))
}

/// Serialize a string-valued enum to its on-disk string (e.g. `LimitMetric::CostUsd` -> "cost_usd").
pub fn enum_to_str<T: Serialize>(v: &T) -> Result<String> {
    serde_json::to_value(v)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| StoreError::Other("enum did not serialize to a string".into()))
}

/// Parse a stored enum string, falling back to the type's default on any mismatch.
pub fn parse_enum<T: DeserializeOwned + Default>(s: &str) -> T {
    serde_json::from_value(Value::String(s.to_string())).unwrap_or_default()
}

/// Serialize a JSON value to a column string, or `None` if it's `Null`.
pub fn json_or_null(v: &Value) -> Result<Option<String>> {
    if v.is_null() {
        Ok(None)
    } else {
        Ok(Some(serde_json::to_string(v)?))
    }
}

/// Parse an optional column string back into a JSON value (`Null` if absent).
pub fn val_or_null(s: Option<String>) -> Result<Value> {
    match s {
        Some(x) => Ok(serde_json::from_str(&x)?),
        None => Ok(Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde::Deserialize;
    use serde_json::json;

    #[test]
    fn fmt_ts_is_fixed_width_nanos_utc() {
        let t = Utc.with_ymd_and_hms(2026, 5, 31, 0, 7, 14).unwrap()
            + chrono::Duration::nanoseconds(110_948_400);
        let s = fmt_ts(t);
        assert_eq!(s, "2026-05-31T00:07:14.110948400Z");
        // Exactly 9 fractional digits + trailing Z => fixed width across all instants.
        assert!(s.ends_with('Z'));
        assert_eq!(s.len(), "2026-05-31T00:07:14.110948400Z".len());
    }

    #[test]
    fn lexicographic_order_matches_chronological_order() {
        // The whole point of the fixed-width format: string `ORDER BY` == time order.
        let earlier = fmt_ts(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
        let later = fmt_ts(
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
                + chrono::Duration::nanoseconds(1),
        );
        let much_later = fmt_ts(Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59).unwrap());
        assert!(earlier < later);
        assert!(later < much_later);
    }

    #[test]
    fn ts_round_trips() {
        let t = Utc.with_ymd_and_hms(2026, 6, 21, 12, 32, 32).unwrap()
            + chrono::Duration::nanoseconds(123_456_789);
        assert_eq!(parse_ts(&fmt_ts(t)).unwrap(), t);
    }

    #[test]
    fn parse_ts_rejects_garbage() {
        assert!(parse_ts("not-a-timestamp").is_err());
    }

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Sample {
        #[default]
        Alpha,
        BetaTwo,
    }

    #[test]
    fn enum_round_trips_and_defaults_on_mismatch() {
        assert_eq!(enum_to_str(&Sample::BetaTwo).unwrap(), "beta_two");
        assert_eq!(parse_enum::<Sample>("beta_two"), Sample::BetaTwo);
        // Unknown / corrupt values fall back to the type default rather than erroring.
        assert_eq!(parse_enum::<Sample>("nonsense"), Sample::Alpha);
    }

    #[test]
    fn json_columns_round_trip_through_null() {
        assert_eq!(json_or_null(&Value::Null).unwrap(), None);
        let v = json!({"a": 1, "b": [true, "x"]});
        let stored = json_or_null(&v).unwrap();
        assert!(stored.is_some());
        assert_eq!(val_or_null(stored).unwrap(), v);
        assert_eq!(val_or_null(None).unwrap(), Value::Null);
    }
}
