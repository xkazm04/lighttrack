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

/// Parse a stored closed-vocabulary string into its domain enum, at the seam, **strictly**.
///
/// This used to be `unwrap_or_default()`: a value the vocabulary did not know was silently coerced
/// into the type's default and handed downstream as if it were a member. That is the one option a
/// mapper never gets — it defers the explosion to whichever caller switches on the value furthest
/// from the evidence, and here the defaults are exactly the ones that read as "fine":
/// `Status::Success` (a corrupt status column becomes a successful call), `Redaction::None`
/// (a corrupt privacy policy becomes "store raw payloads"), and `LimitAction`'s default (a corrupt
/// cap silently changes what it does). None of those look wrong in a dashboard.
///
/// An enum with an explicit quarantine variant — [`Operation::Other`], via `#[serde(other)]` —
/// parses successfully into it and never reaches the error path. That is the technique's *other*
/// sanctioned option, chosen per column: unknown is a value that vocabulary deliberately has. The
/// `provider` column is no longer an enum at all (M8): it is an open id, mapped by
/// `ProviderId::new`, so an unmodeled vendor is neither an error nor a coercion to `unknown`.
///
/// `column` names where the bad value came from, because "invalid enum" without a column is a
/// message nobody can act on. Symmetric with [`parse_ts`], which has always been strict — a
/// timestamp that fails to parse already fails its read, and a status that fails to parse now does
/// too.
pub fn parse_enum<T: DeserializeOwned>(column: &str, s: &str) -> Result<T> {
    serde_json::from_value(Value::String(s.to_string())).map_err(|_| {
        StoreError::Other(format!(
            "stored value {s:?} in column `{column}` is outside its vocabulary — this row was \
             written by something that does not share this schema's enum definition, and coercing \
             it to a default would turn a drift bug into a plausible-looking verdict"
        ))
    })
}

/// Encode a `(ts, id)` keyset position as an opaque, URL/header-safe cursor (hex of `ts|id`). Both
/// components are `|`-free by construction (fixed-width RFC3339 ts, UUID id), so decoding is exact.
/// Shared so every backend mints byte-identical cursors — a page started on one backend's encoding
/// must decode on another after a migration.
pub fn encode_event_cursor(ts: &str, id: &str) -> String {
    let raw = format!("{ts}|{id}");
    raw.bytes().map(|b| format!("{b:02x}")).collect()
}

/// Decode a cursor minted by [`encode_event_cursor`] back into `(ts, id)`; `None` if it isn't valid
/// hex of a `ts|id` pair.
pub fn decode_event_cursor(s: &str) -> Option<(String, String)> {
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect();
    let raw = String::from_utf8(bytes?).ok()?;
    let (ts, id) = raw.split_once('|')?;
    Some((ts.to_string(), id.to_string()))
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
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::nanoseconds(1),
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
    fn a_stored_value_outside_the_vocabulary_is_surfaced_not_coerced() {
        assert_eq!(enum_to_str(&Sample::BetaTwo).unwrap(), "beta_two");
        assert_eq!(
            parse_enum::<Sample>("kind", "beta_two").unwrap(),
            Sample::BetaTwo
        );

        // The behaviour this replaces: `unwrap_or_default()` turned an unknown stored value into
        // `Sample::Alpha` and handed it downstream as if it were a member. On the columns that
        // actually use this, the defaults are the ones that read as "fine" — a corrupt `status`
        // became a SUCCESSFUL call, and a corrupt `redaction` became "store raw payloads".
        let err = parse_enum::<Sample>("kind", "nonsense").expect_err("must not coerce");
        let msg = err.to_string();
        assert!(msg.contains("nonsense"), "the bad value is named: {msg}");
        assert!(msg.contains("kind"), "and the column it came from: {msg}");
    }

    /// The technique's other sanctioned option, chosen per column: a vocabulary that deliberately
    /// HAS an unknown member parses into it and never reaches the error path.
    #[test]
    fn an_explicit_quarantine_variant_still_absorbs_the_unknown() {
        use lighttrack_core::{Operation, ProviderId};
        // The provider column takes the other route: an open id keeps what it was given.
        assert_eq!(ProviderId::new("azure-openai").as_str(), "azure-openai");
        assert_eq!(
            parse_enum::<Operation>("operation", "rerank").unwrap(),
            Operation::Other
        );
    }

    #[test]
    fn event_cursor_round_trips_and_rejects_garbage() {
        let ts = "2026-05-31T00:07:14.110948400Z";
        let id = "ev-123";
        let c = encode_event_cursor(ts, id);
        assert_eq!(
            decode_event_cursor(&c),
            Some((ts.to_string(), id.to_string()))
        );
        assert_eq!(decode_event_cursor(""), None);
        assert_eq!(decode_event_cursor("zz"), None);
        assert_eq!(decode_event_cursor("abc"), None); // odd length
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
