//! `revenue_events` collection + LLM-cost-by-billing-dimension (Phase 1 profit tracking).
//!
//! Firestore has no `OR` predicate or `GROUP BY`/`SUM`, so the recognition-window filter and the
//! cost rollup are done client-side over the project's docs (mirroring `events.rs`'s aggregations).
//! This keeps the backend behaviorally identical to SQLite/Postgres, which the store conformance
//! suite now asserts — without it, this backend would silently inherit the trait's no-op defaults.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use lighttrack_core::{CostByDimension, RevenueEvent, RevenueKind};
use lighttrack_store::Result;

use crate::codec::*;
use crate::rest::Rest;

const COLL: &str = "revenue_events";

pub(crate) fn insert(rest: &Rest, ev: &RevenueEvent) -> Result<()> {
    // Create-or-replace by id, so a webhook redelivery (same id) is an idempotent upsert.
    rest.put_doc(COLL, &ev.id, &to_fields(ev))
}

/// Revenue records recognizable within `[since, until)`, optionally scoped to a project. Mirrors the
/// SQL predicate: a period event overlaps when `period_start < until AND period_end > since`; a
/// point-in-time event (missing either bound) counts when `ts ∈ [since, until)`. Newest first.
pub(crate) fn list(
    rest: &Rest,
    project: Option<&str>,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<RevenueEvent>> {
    let docs = rest.query(COLL, &project_filter(project), None, None)?;
    let mut out: Vec<RevenueEvent> = docs
        .iter()
        .map(from_fields)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|r| recognizable(r, since, until))
        .collect();
    out.sort_by(|a, b| b.ts.cmp(&a.ts));
    Ok(out)
}

/// LLM cost grouped by a billing dimension (`customer` | `product`) read from event metadata, over
/// `[since, until)`. Untagged calls group under a `None` key (the `unattributed` bucket downstream).
pub(crate) fn cost_by_dimension(
    rest: &Rest,
    project: Option<&str>,
    dim: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<CostByDimension>> {
    let field = match dim {
        "product" => "product_id",
        "prompt" => "prompt",
        _ => "customer_id",
    };
    // Push the `[since, until)` window into the query (fixed-width RFC3339 strings make the
    // lexicographic range filter correct — the exact property docs/FIRESTORE.md chose them for, and
    // the same `project_id EQUAL + ts range` shape `events::usage_since` already runs, so this rides
    // the already-required `(project_id, ts)` composite index). Previously only `project_id` was
    // filtered and the window was applied client-side, so every margin/simulate/trend call read the
    // project's ENTIRE event history — billed per doc — however narrow the window.
    let mut filters = project_filter(project);
    filters.push(("ts", "GREATER_THAN_OR_EQUAL", json!(fmt_ts(since))));
    filters.push(("ts", "LESS_THAN", json!(fmt_ts(until))));
    let docs = rest.query("events", &filters, None, None)?;
    let mut agg: BTreeMap<Option<String>, (i64, f64)> = BTreeMap::new();
    for m in &docs {
        // Belt-and-suspenders re-check of the window client-side (also skips ts-less docs).
        let ts = match fstr(m, "ts") {
            Some(s) => parse_ts(&s)?,
            None => continue,
        };
        if ts < since || ts >= until {
            continue;
        }
        let key = metadata_key(m, field)?;
        let e = agg.entry(key).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += ff64(m, "cost_usd").unwrap_or(0.0);
    }
    Ok(agg
        .into_iter()
        .map(|(key, (calls, cost_usd))| CostByDimension { key, calls, cost_usd })
        .collect())
}

fn project_filter(project: Option<&str>) -> Vec<(&str, &str, Value)> {
    match project {
        Some(p) => vec![("project_id", "EQUAL", json!(p))],
        None => vec![],
    }
}

fn recognizable(r: &RevenueEvent, since: DateTime<Utc>, until: DateTime<Utc>) -> bool {
    match (r.period_start, r.period_end) {
        (Some(ps), Some(pe)) => ps < until && pe > since,
        _ => r.ts >= since && r.ts < until,
    }
}

/// Extract `metadata.<field>` (a string) from an event doc; `None` when absent/untagged.
fn metadata_key(m: &Fields, field: &str) -> Result<Option<String>> {
    let meta = fjson(m, "metadata")?;
    Ok(meta.get(field).and_then(Value::as_str).map(str::to_string))
}

fn to_fields(ev: &RevenueEvent) -> Fields {
    let mut m = Fields::new();
    m.insert("id".into(), json!(ev.id));
    m.insert("project_id".into(), json!(ev.project_id));
    m.insert("source".into(), json!(ev.source));
    m.insert("external_id".into(), json!(ev.external_id));
    m.insert("customer_id".into(), json!(ev.customer_id));
    m.insert("product_id".into(), json!(ev.product_id));
    m.insert("amount_usd".into(), json!(ev.amount_usd));
    m.insert("currency".into(), json!(ev.currency));
    m.insert("kind".into(), json!(ev.kind.as_str()));
    m.insert("period_start".into(), json!(ev.period_start.map(fmt_ts)));
    m.insert("period_end".into(), json!(ev.period_end.map(fmt_ts)));
    m.insert("ts".into(), json!(fmt_ts(ev.ts)));
    m
}

fn from_fields(m: &Fields) -> Result<RevenueEvent> {
    Ok(RevenueEvent {
        id: freq(m, "id")?,
        project_id: freq(m, "project_id")?,
        source: freq(m, "source")?,
        external_id: fstr(m, "external_id"),
        customer_id: fstr(m, "customer_id"),
        product_id: fstr(m, "product_id"),
        amount_usd: ff64(m, "amount_usd").unwrap_or(0.0),
        currency: freq(m, "currency")?,
        kind: RevenueKind::parse(&freq(m, "kind")?),
        period_start: fstr(m, "period_start").map(|s| parse_ts(&s)).transpose()?,
        period_end: fstr(m, "period_end").map(|s| parse_ts(&s)).transpose()?,
        ts: parse_ts(&freq(m, "ts")?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode_doc, encode_fields};

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// Push a record through the same typed-value codec the REST client uses (`to_fields` ->
    /// `encode_fields` -> wire doc -> `decode_doc` -> `from_fields`) and back, with no live server.
    /// This is where a backend silently diverges (int-vs-double, null vs absent, ts/kind), so it is
    /// worth pinning even though the full conformance run needs the emulator.
    fn roundtrip(ev: &RevenueEvent) -> RevenueEvent {
        let doc = json!({ "fields": encode_fields(&to_fields(ev)) });
        from_fields(&decode_doc(&doc)).unwrap()
    }

    #[test]
    fn point_in_time_roundtrips() {
        let ev = RevenueEvent {
            id: "r1".into(),
            project_id: "p1".into(),
            source: "stripe".into(),
            external_id: Some("inv-1".into()),
            customer_id: Some("acme".into()),
            product_id: None,
            amount_usd: 20.0, // whole number must survive as f64, not collapse to an integer value
            currency: "USD".into(),
            kind: RevenueKind::OneTime,
            period_start: None,
            period_end: None,
            ts: t("2026-06-10T00:00:00Z"),
        };
        let got = roundtrip(&ev);
        assert_eq!(got.id, "r1");
        assert_eq!(got.external_id.as_deref(), Some("inv-1"));
        assert_eq!(got.customer_id.as_deref(), Some("acme"));
        assert_eq!(got.product_id, None);
        assert!((got.amount_usd - 20.0).abs() < 1e-9);
        assert_eq!(got.kind, RevenueKind::OneTime);
        assert_eq!(got.period_start, None);
        assert_eq!(got.ts, ev.ts);
    }

    #[test]
    fn subscription_period_and_refund_roundtrip() {
        let sub = RevenueEvent {
            id: "r2".into(),
            project_id: "p1".into(),
            source: "polar".into(),
            external_id: None,
            customer_id: None,
            product_id: Some("pro".into()),
            amount_usd: 30.5,
            currency: "EUR".into(),
            kind: RevenueKind::Subscription,
            period_start: Some(t("2026-06-01T00:00:00Z")),
            period_end: Some(t("2026-07-01T00:00:00Z")),
            ts: t("2026-06-01T00:00:00Z"),
        };
        let got = roundtrip(&sub);
        assert_eq!(got.kind, RevenueKind::Subscription);
        assert_eq!(got.product_id.as_deref(), Some("pro"));
        assert_eq!(got.currency, "EUR");
        assert_eq!(got.period_start, sub.period_start);
        assert_eq!(got.period_end, sub.period_end);

        let refund = RevenueEvent { kind: RevenueKind::Refund, ..sub };
        assert_eq!(roundtrip(&refund).kind, RevenueKind::Refund);
    }

    #[test]
    fn recognition_window_matches_sql_predicate() {
        let s = t("2026-06-01T00:00:00Z");
        let u = t("2026-07-01T00:00:00Z");
        let point = |ts: &str| RevenueEvent {
            id: "x".into(), project_id: "p".into(), source: "manual".into(),
            external_id: None, customer_id: None, product_id: None, amount_usd: 1.0,
            currency: "USD".into(), kind: RevenueKind::OneTime,
            period_start: None, period_end: None, ts: t(ts),
        };
        assert!(recognizable(&point("2026-06-15T00:00:00Z"), s, u), "in-window point counts");
        assert!(!recognizable(&point("2026-05-15T00:00:00Z"), s, u), "pre-window point excluded");
        assert!(!recognizable(&point("2026-07-01T00:00:00Z"), s, u), "until is exclusive");

        // A period that merely overlaps the window is recognizable; one entirely before is not.
        let mut overlap = point("2026-05-20T00:00:00Z");
        overlap.period_start = Some(t("2026-05-20T00:00:00Z"));
        overlap.period_end = Some(t("2026-06-05T00:00:00Z"));
        assert!(recognizable(&overlap, s, u), "period overlapping the window counts");
        overlap.period_end = Some(t("2026-05-25T00:00:00Z"));
        assert!(!recognizable(&overlap, s, u), "period entirely before the window excluded");
    }
}
