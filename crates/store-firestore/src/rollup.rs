//! The one grouped rollup, Firestore: a client-side fold, because the REST API has no `GROUP BY`.
//!
//! The window and the project are pushed to the server (the same `project_id EQUAL + ts` range the
//! other aggregates use, so it rides the already-required composite index); the grouping, the
//! metadata extraction and the filters are applied to the returned documents. That is how every
//! aggregate on this backend already works — this one replaces five of them.
//!
//! **`TimeKey::ReceivedAt` falls back to `ts` here.** Firestore documents carry no `received_at`
//! field, so a window asked for on server-arrival time is answered on client-declared time. The row
//! carries no marker saying so, which matters: on this backend a caller with a backdated clock can
//! shift its own spend between forecast buckets. Postgres and SQLite key accounting reads on real
//! arrival time; this is one more reason `docs/PARITY.md` calls Firestore's caps advisory.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use lighttrack_core::{Dimension, RollupQuery, RollupRow, Storage};
use lighttrack_store::{Result, StoreError};

use crate::codec::{ff64, fi64, fjson, fmt_ts, fstr, Fields};
use crate::rest::Rest;

const COLL: &str = "events";

/// One document's value on a dimension. Columns are top-level fields; the rest ride inside the
/// JSON-encoded `metadata` string, exactly as the SQL backends extract them. `None` means the
/// document carries no value there — it folds into the NULL bucket and matches no filter on it.
fn value_of(m: &Fields, d: Dimension, day: Option<&str>) -> Result<Option<String>> {
    Ok(match d.storage() {
        Storage::Column("project_id") => fstr(m, "project_id"),
        Storage::Column(c) => fstr(m, c),
        Storage::MetadataKey(k) => fjson(m, "metadata")?
            .get(k)
            .and_then(Value::as_str)
            .map(str::to_string),
        Storage::Day => day.map(str::to_string),
    })
}

pub(crate) fn rollup(rest: &Rest, q: &RollupQuery<'_>) -> Result<Vec<RollupRow>> {
    if let Some(why) = q.invalid() {
        return Err(StoreError::Other(why));
    }
    // `ts` on both bounds: there is no `received_at` here, so an accounting window is answered on
    // the client's declared time (see the module note).
    let mut filters: Vec<(&str, &str, Value)> = match q.project {
        Some(p) => vec![("project_id", "EQUAL", json!(p))],
        None => vec![],
    };
    filters.push(("ts", "GREATER_THAN_OR_EQUAL", json!(fmt_ts(q.since))));
    if let Some(u) = q.until {
        filters.push(("ts", "LESS_THAN", json!(fmt_ts(u))));
    }
    let docs = rest.query(COLL, &filters, None, None)?;

    let mut agg: BTreeMap<Vec<Option<String>>, RollupRow> = BTreeMap::new();
    for m in &docs {
        // The `YYYY-MM-DD` prefix of the fixed-width RFC3339 timestamp is the UTC calendar day.
        let day = fstr(m, "ts").map(|s| s.chars().take(10).collect::<String>());
        let day = day.as_deref();

        let mut skip = false;
        for (d, want) in &q.filter {
            if value_of(m, *d, day)?.as_deref() != Some(want.as_str()) {
                skip = true;
                break;
            }
        }
        if skip {
            continue;
        }

        let mut keys = Vec::with_capacity(q.group_by.len());
        for d in &q.group_by {
            keys.push(value_of(m, *d, day)?);
        }
        let row = agg
            .entry(keys.clone())
            .or_insert_with(|| RollupRow::empty(keys));
        fold(row, m)?;
    }
    Ok(agg.into_values().collect())
}

/// Fold one document into its bucket, mirroring the SQL aggregates. A document with no `cost_usd`
/// is *unpriced*: counted, never summed as `$0.00` — we do not invent a price we don't have.
fn fold(row: &mut RollupRow, m: &Fields) -> Result<()> {
    row.calls += 1;
    row.input_tokens += fi64(m, "input_tokens").unwrap_or(0).max(0) as u64;
    row.output_tokens += fi64(m, "output_tokens").unwrap_or(0).max(0) as u64;
    match ff64(m, "cost_usd") {
        Some(c) => {
            row.cost_usd += c;
            if fjson(m, "metadata")?
                .get("cost_source")
                .and_then(Value::as_str)
                == Some("client")
            {
                row.client_reported_cost_usd += c;
            }
        }
        None => row.unpriced_calls += 1,
    }
    if fstr(m, "status").as_deref() != Some("success") {
        row.errors += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(pairs: &[(&str, Value)]) -> Fields {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn a_dimension_reads_from_a_column_or_from_metadata() {
        let m = doc(&[
            ("provider", json!("openai")),
            ("model", json!("gpt-5.4")),
            ("metadata", json!(r#"{"customer_id":"acme"}"#)),
        ]);
        assert_eq!(
            value_of(&m, Dimension::Provider, None).expect("ok"),
            Some("openai".into())
        );
        assert_eq!(
            value_of(&m, Dimension::Customer, None).expect("ok"),
            Some("acme".into())
        );
        // A dimension the document carries no value on is `None`, never an empty string: the two
        // would land in different buckets and the parts would stop summing to the whole.
        assert_eq!(value_of(&m, Dimension::Name, None).expect("ok"), None);
        assert_eq!(value_of(&m, Dimension::Product, None).expect("ok"), None);
        assert_eq!(
            value_of(&m, Dimension::Day, Some("2026-06-10")).expect("ok"),
            Some("2026-06-10".into())
        );
    }

    #[test]
    fn an_unpriced_document_is_counted_not_priced_at_zero() {
        let mut row = RollupRow::empty(vec![None]);
        fold(
            &mut row,
            &doc(&[
                ("input_tokens", json!(10)),
                ("output_tokens", json!(5)),
                ("status", json!("success")),
            ]),
        )
        .expect("fold");
        assert_eq!(row.calls, 1);
        assert_eq!(row.unpriced_calls, 1);
        assert_eq!(row.cost_usd, 0.0, "no invented price");
        assert_eq!(row.tokens(), 15);
        assert_eq!(row.errors, 0);
    }

    #[test]
    fn the_client_reported_share_and_errors_are_separated_out() {
        let mut row = RollupRow::empty(vec![None]);
        fold(
            &mut row,
            &doc(&[
                ("cost_usd", json!(2.0)),
                ("status", json!("error")),
                ("metadata", json!(r#"{"cost_source":"client"}"#)),
            ]),
        )
        .expect("fold");
        fold(
            &mut row,
            &doc(&[("cost_usd", json!(1.0)), ("status", json!("success"))]),
        )
        .expect("fold");
        assert_eq!(row.calls, 2);
        assert!((row.cost_usd - 3.0).abs() < 1e-9);
        assert!(
            (row.client_reported_cost_usd - 2.0).abs() < 1e-9,
            "only the self-reported call's cost"
        );
        assert_eq!(row.errors, 1, "a failed call still cost money");
    }
}
