//! `get_limit_status` (live per-rule usage vs threshold) and `list_limits` (configured rules).

use serde_json::Value;

use crate::md::{commafy, f, money, pct, s, Align, Table};

pub(crate) fn status(v: &Value) -> Option<String> {
    let statuses = v.get("statuses")?.as_array()?;
    let project = s(v, "project_id");
    let throttled = v.get("throttled").and_then(Value::as_bool).unwrap_or(false);
    if statuses.is_empty() {
        return Some(format!("_No limit rules configured for `{project}`._"));
    }
    let mut t = Table::new(&[
        ("Metric", Align::Left),
        ("Window", Align::Left),
        ("Used", Align::Right),
        ("Threshold", Align::Right),
        ("Used %", Align::Right),
        ("Status", Align::Left),
    ]);
    for st in statuses {
        let metric = s(st, "metric");
        let current = f(st, "current");
        let threshold = f(st, "threshold");
        let ratio = f(st, "ratio");
        let breached = st.get("breached").and_then(Value::as_bool).unwrap_or(false);
        // Prefer the rule's explicit soft-warning flag; fall back to the 0.8 heuristic for rules
        // that configured no warn_at.
        let warning = st.get("warning").and_then(Value::as_bool).unwrap_or(false);
        let (used, thr) = if metric == "cost_usd" {
            (money(current), money(threshold))
        } else {
            (commafy(current as u64), commafy(threshold as u64))
        };
        let badge = if breached {
            "❌ over"
        } else if warning || ratio >= 0.8 {
            "⚠️ warning"
        } else {
            "✅ ok"
        };
        t.row(vec![
            metric.to_string(),
            s(st, "window").to_string(),
            used,
            thr,
            pct(ratio),
            badge.to_string(),
        ]);
    }
    let header = if throttled {
        format!("### Limits — `{project}` ⚠️ **throttled**\n\n")
    } else {
        format!("### Limits — `{project}` ✅ within limits\n\n")
    };
    let mut out = format!("{header}{}", t.render());
    if let Some(rejected) = rejected_table(v) {
        out.push_str("\n\n");
        out.push_str(&rejected);
    }
    Some(out)
}

/// Best-effort rejected-traffic ledger (process-local, 24h rolling): calls the caps turned away with
/// their estimated missed cost. Only rendered when the `rejected` block is present and non-empty.
fn rejected_table(v: &Value) -> Option<String> {
    let rows = v.get("rejected")?.as_array()?;
    if rows.is_empty() {
        return None;
    }
    let mut t = Table::new(&[
        ("Metric", Align::Left),
        ("Window", Align::Left),
        ("Rejected", Align::Right),
        ("Est. missed $", Align::Right),
    ]);
    for r in rows {
        t.row(vec![
            s(r, "metric").to_string(),
            s(r, "window").to_string(),
            commafy(f(r, "count") as u64),
            money(f(r, "est_missed_cost_usd")),
        ]);
    }
    Some(format!(
        "**Rejected traffic** (last 24h, best-effort; resets on restart)\n\n{}",
        t.render()
    ))
}

pub(crate) fn list(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_No limit rules._".to_string());
    }
    let mut t = Table::new(&[
        ("Metric", Align::Left),
        ("Window", Align::Left),
        ("Threshold", Align::Right),
        ("Warn at", Align::Right),
        ("Action", Align::Left),
        ("Enabled", Align::Left),
    ]);
    for r in rows {
        let metric = s(r, "metric");
        let threshold = f(r, "threshold");
        let thr = if metric == "cost_usd" {
            money(threshold)
        } else {
            commafy(threshold as u64)
        };
        // warn_at is an optional fraction of the threshold; show it as a percentage or an em dash.
        let warn = r
            .get("warn_at")
            .and_then(Value::as_f64)
            .map(|w| pct(w))
            .unwrap_or_else(|| "—".to_string());
        let enabled = r.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        t.row(vec![
            metric.to_string(),
            s(r, "window").to_string(),
            thr,
            warn,
            s(r, "action").to_string(),
            if enabled { "✅".into() } else { "—".into() },
        ]);
    }
    Some(t.render())
}
