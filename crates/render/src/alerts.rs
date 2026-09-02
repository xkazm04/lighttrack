//! `list_alerts` — the fired-alert ledger table.
//!
//! The column that matters most here is **Delivery**. Before the ledger, an alert was a webhook POST
//! and a log line, so "nobody was told" and "everybody was told" looked identical from the outside.
//! This table makes the undelivered case loud: `∅ none` for an alert that reached no channel at all,
//! `❌ 1/2` when a channel refused it, `✅ 2/2` only when every attempt succeeded.

use serde_json::Value;

use crate::md::{opt_s, s, short_ts, trunc, Align, Table};

pub(crate) fn list(v: &Value) -> Option<String> {
    // The API wraps the page as `{ "alerts": [...], "next_cursor": ... }`; a bare array is accepted
    // too so a caller that already unwrapped the envelope still renders.
    let rows = match v {
        Value::Array(a) => a,
        _ => v.get("alerts")?.as_array()?,
    };
    if rows.is_empty() {
        return Some("_No alerts — nothing has fired in this window._".to_string());
    }
    let mut t = Table::new(&[
        ("Fired", Align::Left),
        ("Kind", Align::Left),
        ("Sev", Align::Left),
        ("Project", Align::Left),
        ("Message", Align::Left),
        ("Delivery", Align::Left),
        ("Acked by", Align::Left),
    ]);
    let mut undelivered = 0usize;
    for r in rows {
        let d = delivery(r.get("delivered"));
        if d.silent {
            undelivered += 1;
        }
        t.row(vec![
            short_ts(s(r, "fired_at")),
            s(r, "kind").to_string(),
            severity_badge(s(r, "severity")).to_string(),
            project_label(s(r, "project_id")),
            message(r),
            d.label,
            acked(r),
        ]);
    }
    let mut out = t.render();
    if undelivered > 0 {
        // Surfaced above the fold as well as in the column: an operator scanning a long page should
        // not have to notice a glyph to learn that some of these reached nobody.
        out.push_str(&format!(
            "\n⚠️ **{undelivered} of {} reached no channel** — check alert routing.\n",
            rows.len()
        ));
    }
    Some(out)
}

struct DeliveryCell {
    label: String,
    /// Nothing reached a human: no attempt at all, or every attempt failed.
    silent: bool,
}

fn delivery(v: Option<&Value>) -> DeliveryCell {
    let attempts = v
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if attempts.is_empty() {
        return DeliveryCell {
            label: "∅ none".into(),
            silent: true,
        };
    }
    let ok = attempts
        .iter()
        .filter(|d| d.get("ok").and_then(Value::as_bool).unwrap_or(false))
        .count();
    let n = attempts.len();
    let glyph = if ok == n {
        "✅"
    } else if ok == 0 {
        "∅"
    } else {
        "❌"
    };
    DeliveryCell {
        label: format!("{glyph} {ok}/{n} ok"),
        silent: ok == 0,
    }
}

fn severity_badge(sev: &str) -> &'static str {
    match sev {
        "critical" => "❌ crit",
        "warning" => "⚠️ warn",
        _ => "· info",
    }
}

/// The human-readable line, if the payload carries one. Payload shapes differ per kind, so fall back
/// to the dedup key — which always names the condition — rather than rendering an empty cell.
fn message(r: &Value) -> String {
    let text = r
        .get("payload")
        .and_then(|p| {
            ["text", "message", "summary", "reason"]
                .into_iter()
                .find_map(|k| p.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()))
        })
        .unwrap_or_else(|| s(r, "dedup_key"));
    trunc(text, 48)
}

fn acked(r: &Value) -> String {
    match opt_s(r, "acked_by").filter(|v| !v.is_empty()) {
        Some(by) => trunc(by, 18),
        // Distinguish "acknowledged, but by nobody named" from "still open".
        None if r.get("acked_at").map(|v| !v.is_null()).unwrap_or(false) => "✓".into(),
        None => "—".into(),
    }
}

/// A deployment-wide alert has no project; say so rather than leaving the cell blank.
fn project_label(v: &str) -> String {
    if v.is_empty() {
        "— global".into()
    } else {
        trunc(v, 20)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn alert(delivered: Value) -> Value {
        json!({
            "id": "a1",
            "project_id": "proj-a",
            "kind": "limit_breach",
            "dedup_key": "proj-a:cost_usd:hour",
            "severity": "critical",
            "payload": { "text": "cost_usd over 10.00 for the hour window" },
            "fired_at": "2026-09-01T12:34:56.000000000Z",
            "delivered": delivered
        })
    }

    #[test]
    fn an_alert_nobody_received_is_visually_distinct_from_a_delivered_one() {
        let d = delivery(Some(&json!([])));
        assert_eq!(d.label, "∅ none");
        assert!(d.silent);

        let d = delivery(Some(&json!([{ "channel_id": "env:webhook", "ok": true }])));
        assert_eq!(d.label, "✅ 1/1 ok");
        assert!(!d.silent);

        let d = delivery(Some(&json!([
            { "channel_id": "c1", "ok": true },
            { "channel_id": "c2", "ok": false }
        ])));
        assert_eq!(d.label, "❌ 1/2 ok");
        assert!(!d.silent, "one channel did receive it");

        let d = delivery(Some(&json!([{ "channel_id": "c1", "ok": false }])));
        assert_eq!(d.label, "∅ 0/1 ok");
        assert!(d.silent, "every attempt failed: nobody was reached");

        assert!(delivery(None).silent);
    }

    #[test]
    fn the_table_warns_when_any_row_reached_nobody() {
        let out = list(&json!({
            "alerts": [alert(json!([])), alert(json!([{ "channel_id": "c", "ok": true }]))],
            "next_cursor": null
        }))
        .expect("renders");
        assert!(out.contains("limit_breach"), "{out}");
        assert!(out.contains("∅ none"), "{out}");
        assert!(
            out.contains("1 of 2 reached no channel"),
            "the silent case must be called out: {out}"
        );
    }

    #[test]
    fn a_fully_delivered_page_carries_no_warning_banner() {
        let out = list(&json!({
            "alerts": [alert(json!([{ "channel_id": "c", "ok": true }]))]
        }))
        .expect("renders");
        assert!(!out.contains("reached no channel"), "{out}");
        assert!(out.contains("✅ 1/1 ok"), "{out}");
    }

    #[test]
    fn message_falls_back_to_the_dedup_key_and_project_to_global() {
        let mut a = alert(json!([]));
        a["payload"] = json!({ "breached": true });
        assert_eq!(message(&a), "proj-a:cost_usd:hour");
        a["payload"] = json!({ "summary": "error rate spiked" });
        assert_eq!(message(&a), "error rate spiked");
        assert_eq!(project_label(""), "— global");
    }

    #[test]
    fn ack_state_reads_as_open_named_or_anonymous() {
        let mut a = alert(json!([]));
        assert_eq!(acked(&a), "—");
        a["acked_at"] = json!("2026-09-01T13:00:00.000000000Z");
        assert_eq!(acked(&a), "✓");
        a["acked_by"] = json!("oncall-mia");
        assert_eq!(acked(&a), "oncall-mia");
    }

    #[test]
    fn severity_maps_to_a_badge_and_an_empty_page_says_so() {
        assert_eq!(severity_badge("critical"), "❌ crit");
        assert_eq!(severity_badge("warning"), "⚠️ warn");
        assert_eq!(severity_badge("whatever"), "· info");
        let out = list(&json!({ "alerts": [] })).expect("renders");
        assert!(out.contains("No alerts"), "{out}");
        assert!(list(&json!({})).is_none(), "no alerts key is not a listing");
    }

    #[test]
    fn a_bare_array_renders_too() {
        let out =
            list(&json!([alert(json!([{ "channel_id": "c", "ok": true }]))])).expect("renders");
        assert!(out.contains("proj-a"), "{out}");
    }
}
