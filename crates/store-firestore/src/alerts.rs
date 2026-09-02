//! The persisted alert ledger on Firestore: `alerts` + an `alert_dedup` guard collection.
//!
//! **How the cooldown gate is made atomic without transactions.** Firestore's REST surface gives two
//! primitives this needs: `create` with `currentDocument.exists=false`, and `commit` with an
//! `updateTime` precondition. One guard document per dedup key (`alert_dedup/<hex(key)>`) turns the
//! gate into a compare-and-set:
//!
//! * no guard doc → create it with `exists=false`. A losing racer gets `ALREADY_EXISTS`, which is a
//!   suppression, not an error.
//! * a guard doc still inside the cooldown → suppressed outright, no write.
//! * a guard doc past the cooldown → take it over with an `updateTime` precondition. A losing racer
//!   gets `FAILED_PRECONDITION`, which is again a suppression.
//!
//! Only the winner writes the alert row, so two replicas produce one alert — which is the whole
//! reason the ledger is a store concern rather than a process's `HashMap`.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use lighttrack_core::{Alert, AlertKind, Delivery, Severity};
use lighttrack_store::codec::{decode_event_cursor, fmt_ts, parse_ts};
use lighttrack_store::{AlertAdmission, AlertFilter, Result, StoreError};

use crate::codec::*;
use crate::rest::Rest;

const COLL: &str = "alerts";
const DEDUP: &str = "alert_dedup";

/// A Firestore document id for an arbitrary dedup key. Hex rather than the key itself because a
/// document id may not contain `/` and a dedup key is free-form; hex is reversible, so a guard doc
/// can still be traced back to the condition that wrote it.
fn dedup_doc_id(key: &str) -> String {
    key.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn insert_alert_dedup(
    rest: &Rest,
    a: &Alert,
    cooldown: Duration,
) -> Result<AlertAdmission> {
    let guard_id = dedup_doc_id(&a.dedup_key);
    let cutoff = a.fired_at
        - chrono::Duration::from_std(cooldown).unwrap_or_else(|_| chrono::Duration::zero());

    let mut guard = Fields::new();
    guard.insert("dedup_key".into(), json!(a.dedup_key));
    guard.insert("fired_at".into(), json!(fmt_ts(a.fired_at)));
    guard.insert("alert_id".into(), json!(a.id));

    let existing = rest.query_raw(
        DEDUP,
        &[("dedup_key", "EQUAL", json!(a.dedup_key))],
        None,
        Some(1),
    )?;
    let won = match existing.first() {
        None => match rest.create_doc(DEDUP, &guard_id, &guard) {
            Ok(()) => true,
            // Another writer created the guard between our read and our create. That is exactly the
            // race this gate exists for, and losing it is a suppression rather than a failure.
            Err(StoreError::Conflict(_)) => false,
            Err(e) => return Err(e),
        },
        Some(doc) => {
            let fields = decode_doc(doc);
            let prev = fstr(&fields, "fired_at")
                .map(|s| parse_ts(&s))
                .transpose()?;
            match prev {
                Some(t) if !cooldown.is_zero() && t > cutoff => {
                    return Ok(AlertAdmission::Suppressed { fired_at: t })
                }
                _ => {
                    let update_time = doc.get("updateTime").and_then(|v| v.as_str());
                    rest.commit_update(
                        &rest.doc_name(DEDUP, &guard_id),
                        &guard,
                        &["dedup_key", "fired_at", "alert_id"],
                        update_time,
                    )?
                }
            }
        }
    };
    if !won {
        // The winner's `fired_at` is what a caller wants named. Re-read it; if it has already gone
        // (a retention sweep, a concurrent delete) report the instant we tried, which is within a
        // scheduling quantum of the truth and never claims the alert was sent.
        let fired_at = rest
            .get_doc(DEDUP, &guard_id)?
            .and_then(|f| fstr(&f, "fired_at"))
            .map(|s| parse_ts(&s))
            .transpose()?
            .unwrap_or(a.fired_at);
        return Ok(AlertAdmission::Suppressed { fired_at });
    }
    rest.put_doc(COLL, &a.id, &alert_fields(a)?)?;
    Ok(AlertAdmission::Admitted)
}

pub(crate) fn mark_delivery(rest: &Rest, alert_id: &str, d: &Delivery) -> Result<bool> {
    let Some(doc) = rest.get_doc(COLL, alert_id)? else {
        return Ok(false);
    };
    let mut list = parse_deliveries(fstr(&doc, "delivered"));
    list.push(d.clone());
    let mut f = Fields::new();
    f.insert("delivered".into(), json!(serde_json::to_string(&list)?));
    rest.patch_fields(COLL, alert_id, &f, &["delivered"])?;
    Ok(true)
}

pub(crate) fn get_alert(rest: &Rest, id: &str) -> Result<Option<Alert>> {
    rest.get_doc(COLL, id)?.as_ref().map(alert_from).transpose()
}

pub(crate) fn ack_alert(rest: &Rest, id: &str, by: &str, at: DateTime<Utc>) -> Result<bool> {
    if rest.get_doc(COLL, id)?.is_none() {
        return Ok(false);
    }
    let mut f = Fields::new();
    f.insert("acked_at".into(), json!(fmt_ts(at)));
    f.insert("acked_by".into(), json!(by));
    // A denormalized boolean so `?acked=` is a server-side equality rather than a client-side
    // filter that would silently return short pages.
    f.insert("acked".into(), json!(true));
    rest.patch_fields(COLL, id, &f, &["acked_at", "acked_by", "acked"])?;
    Ok(true)
}

pub(crate) fn attach_alert_resolution(rest: &Rest, id: &str, resolution: &Value) -> Result<bool> {
    if rest.get_doc(COLL, id)?.is_none() {
        return Ok(false);
    }
    let mut f = Fields::new();
    f.insert(
        "resolution".into(),
        json!(serde_json::to_string(resolution)?),
    );
    rest.patch_fields(COLL, id, &f, &["resolution"])?;
    Ok(true)
}

pub(crate) fn list_alerts(rest: &dyn AlertQuery, f: &AlertFilter) -> Result<Vec<Alert>> {
    let mut filters: Vec<(&str, &str, Value)> = Vec::new();
    if let Some(p) = &f.project {
        filters.push(("project_id", "EQUAL", json!(p)));
    }
    if let Some(k) = f.kind {
        filters.push(("kind", "EQUAL", json!(k.as_str())));
    }
    if let Some(since) = f.since {
        filters.push(("fired_at", "GREATER_THAN_OR_EQUAL", json!(fmt_ts(since))));
    }
    if let Some(acked) = f.acked {
        filters.push(("acked", "EQUAL", json!(acked)));
    }
    let cursor = f.cursor.as_deref().and_then(decode_event_cursor);
    if let Some((ts, _)) = &cursor {
        // `<=`, not `<`: the id tiebreak is applied below, because Firestore cannot express the
        // `(ts < c) OR (ts = c AND id < i)` disjunction the keyset needs.
        filters.push(("fired_at", "LESS_THAN_OR_EQUAL", json!(ts)));
    }
    let want = f.effective_limit();
    // Over-fetch a little so the tiebreak below cannot hand back a short page.
    let fetch = want.saturating_add(8);
    let docs = rest.query_alerts(&filters, Some(("fired_at", true)), Some(fetch))?;
    let mut out = Vec::new();
    for d in &docs {
        let a = alert_from(d)?;
        if let Some((ts, id)) = &cursor {
            let a_ts = fmt_ts(a.fired_at);
            if a_ts > *ts || (a_ts == *ts && a.id.as_str() >= id.as_str()) {
                continue;
            }
        }
        out.push(a);
        if out.len() == want {
            break;
        }
    }
    Ok(out)
}

/// The one query shape the listing needs, behind a trait so the cursor/tiebreak logic above is
/// unit-testable without a live Firestore.
pub(crate) trait AlertQuery {
    fn query_alerts(
        &self,
        filters: &[(&str, &str, Value)],
        order: Option<(&str, bool)>,
        limit: Option<usize>,
    ) -> Result<Vec<Fields>>;
}

impl AlertQuery for Rest {
    fn query_alerts(
        &self,
        filters: &[(&str, &str, Value)],
        order: Option<(&str, bool)>,
        limit: Option<usize>,
    ) -> Result<Vec<Fields>> {
        self.query(COLL, filters, order, limit)
    }
}

fn alert_fields(a: &Alert) -> Result<Fields> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(a.id));
    m.insert("project_id".into(), json!(a.project_id));
    m.insert("kind".into(), json!(a.kind.as_str()));
    m.insert("dedup_key".into(), json!(a.dedup_key));
    m.insert("severity".into(), json!(a.severity.as_str()));
    m.insert("payload".into(), json!(serde_json::to_string(&a.payload)?));
    m.insert("fired_at".into(), json!(fmt_ts(a.fired_at)));
    m.insert(
        "delivered".into(),
        json!(serde_json::to_string(&a.delivered)?),
    );
    m.insert("acked_at".into(), json!(a.acked_at.map(fmt_ts)));
    m.insert("acked_by".into(), json!(a.acked_by));
    m.insert("acked".into(), json!(a.acked_at.is_some()));
    m.insert("resolution".into(), json!(opt_json_str(&a.resolution)?));
    Ok(m)
}

fn parse_deliveries(raw: Option<String>) -> Vec<Delivery> {
    raw.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn alert_from(m: &Fields) -> Result<Alert> {
    let id: String = freq(m, "id")?;
    let kind_raw = fstr(m, "kind").unwrap_or_default();
    let kind = AlertKind::from_wire(&kind_raw).ok_or_else(|| {
        StoreError::Other(format!("alert '{id}' carries an unknown kind '{kind_raw}'"))
    })?;
    let fired_at = fstr(m, "fired_at").ok_or_else(|| missing("fired_at"))?;
    Ok(Alert {
        id,
        project_id: fstr(m, "project_id"),
        kind,
        dedup_key: fstr(m, "dedup_key").unwrap_or_default(),
        severity: Severity::from_wire(&fstr(m, "severity").unwrap_or_default()),
        payload: fstr(m, "payload")
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null),
        fired_at: parse_ts(&fired_at)?,
        delivered: parse_deliveries(fstr(m, "delivered")),
        acked_at: fstr(m, "acked_at").as_deref().map(parse_ts).transpose()?,
        acked_by: fstr(m, "acked_by"),
        resolution: fstr(m, "resolution").and_then(|s| serde_json::from_str(&s).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Vec<Alert>);

    impl AlertQuery for Fake {
        fn query_alerts(
            &self,
            _filters: &[(&str, &str, Value)],
            _order: Option<(&str, bool)>,
            _limit: Option<usize>,
        ) -> Result<Vec<Fields>> {
            self.0.iter().map(alert_fields).collect()
        }
    }

    fn alert(id: &str, secs: i64) -> Alert {
        let mut a = Alert::new(
            AlertKind::LimitBreach,
            Some("p1".into()),
            "p1:cost".into(),
            json!({}),
        );
        a.id = id.into();
        a.fired_at = DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap_or_else(Utc::now);
        a
    }

    /// The keyset's id tiebreak is applied client-side here, so it has to be right: paging past a
    /// row must not hand that row back, and must not skip the one after it.
    #[test]
    fn the_cursor_excludes_the_row_it_names_and_nothing_else() {
        // Newest first, as the query orders them.
        let fake = Fake(vec![alert("c", 30), alert("b", 20), alert("a", 10)]);
        let page = list_alerts(
            &fake,
            &AlertFilter {
                limit: 1,
                ..Default::default()
            },
        )
        .expect("first page");
        assert_eq!(page[0].id, "c");

        let cursor = lighttrack_store::codec::encode_event_cursor(&fmt_ts(page[0].fired_at), "c");
        let next = list_alerts(
            &fake,
            &AlertFilter {
                limit: 2,
                cursor: Some(cursor),
                ..Default::default()
            },
        )
        .expect("second page");
        assert_eq!(
            next.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "a"]
        );
    }

    /// Two alerts fired in the same instant differ only by id, and the tiebreak has to separate them
    /// — this is the case a bare `fired_at <` cursor silently drops.
    #[test]
    fn a_same_instant_pair_is_separated_by_the_id_tiebreak() {
        let fake = Fake(vec![alert("b", 10), alert("a", 10)]);
        let cursor = lighttrack_store::codec::encode_event_cursor(&fmt_ts(fake.0[0].fired_at), "b");
        let next = list_alerts(
            &fake,
            &AlertFilter {
                cursor: Some(cursor),
                ..Default::default()
            },
        )
        .expect("page");
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].id, "a");
    }
}
