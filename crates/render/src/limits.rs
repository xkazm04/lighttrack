//! `get_limit_status` (live per-rule usage vs threshold) and `list_limits` (configured rules).

use serde_json::Value;

use crate::md::{commafy, f, money, pct, s, Align, Table};

/// Render a rule/status's optional `scope` object (`{"model":"gpt-4o"}`) as a compact `kind=value`,
/// or an em dash when the rule is project-wide (unscoped).
fn scope_label(v: &Value) -> String {
    match v
        .get("scope")
        .and_then(Value::as_object)
        .and_then(|m| m.iter().next())
    {
        Some((kind, val)) => format!("{kind}={}", val.as_str().unwrap_or_default()),
        None => "—".to_string(),
    }
}

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
        ("Scope", Align::Left),
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
        // The API's `warning` is the rule's own warn_at verdict and wins when present; the 0.8
        // heuristic is only for a status that carries no flag at all. `unwrap_or(false) || ratio >=
        // 0.8` let the heuristic override a rule whose warn_at was deliberately set higher.
        let warning = st
            .get("warning")
            .and_then(Value::as_bool)
            .unwrap_or(ratio >= 0.8);
        let (used, thr) = if metric == "cost_usd" {
            (money(current), money(threshold))
        } else {
            (commafy(current as u64), commafy(threshold as u64))
        };
        let badge = if breached {
            "❌ over"
        } else if warning {
            "⚠️ warning"
        } else {
            "✅ ok"
        };
        t.row(vec![
            metric.to_string(),
            s(st, "window").to_string(),
            scope_label(st),
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

/// A rule's threshold: a number is a fixed cap in the metric's unit; an object is a revenue-share
/// cap (`{"pct": 80, ..}`) resolved per customer at evaluation time, which used to read as `$0`
/// because the number accessor saw an object.
fn threshold_cell(metric: &str, threshold: Option<&Value>) -> String {
    match threshold {
        Some(Value::Object(o)) => format!(
            "{}% of revenue",
            o.get("pct").and_then(Value::as_f64).unwrap_or(0.0)
        ),
        Some(v) => {
            let x = v.as_f64().unwrap_or(0.0);
            if metric == "cost_usd" {
                money(x)
            } else {
                commafy(x as u64)
            }
        }
        None => "—".to_string(),
    }
}

pub(crate) fn list(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_No limit rules._".to_string());
    }
    let mut t = Table::new(&[
        ("Metric", Align::Left),
        ("Window", Align::Left),
        ("Scope", Align::Left),
        ("Threshold", Align::Right),
        ("Warn at", Align::Right),
        ("Action", Align::Left),
        ("Enabled", Align::Left),
    ]);
    for r in rows {
        let metric = s(r, "metric");
        let thr = threshold_cell(metric, r.get("threshold"));
        // warn_at is an optional fraction of the threshold; show it as a percentage or an em dash.
        let warn = r
            .get("warn_at")
            .and_then(Value::as_f64)
            .map(pct)
            .unwrap_or_else(|| "—".to_string());
        let enabled = r.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        t.row(vec![
            metric.to_string(),
            s(r, "window").to_string(),
            scope_label(r),
            thr,
            warn,
            s(r, "action").to_string(),
            if enabled { "✅".into() } else { "—".into() },
        ]);
    }
    Some(t.render())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A rule that set warn_at above 80% is not second-guessed by the renderer's fallback.
    #[test]
    fn the_apis_warning_verdict_wins_over_the_heuristic() {
        let md = status(
            &json!({ "project_id": "p", "throttled": false, "statuses": [
                { "metric": "cost_usd", "window": "day", "current": 8.5, "threshold": 10.0,
                  "ratio": 0.85, "breached": false, "warning": false }
            ]}),
        )
        .unwrap();
        assert!(md.contains("✅ ok"), "{md}");
        let md = status(
            &json!({ "project_id": "p", "throttled": false, "statuses": [
                { "metric": "cost_usd", "window": "day", "current": 8.5, "threshold": 10.0,
                  "ratio": 0.85, "breached": false }
            ]}),
        )
        .unwrap();
        assert!(md.contains("⚠️ warning"), "no flag → heuristic: {md}");
    }

    #[test]
    fn a_revenue_share_threshold_is_not_rendered_as_zero_dollars() {
        let md = list(&json!([
            { "metric": "cost_usd", "window": "month", "threshold": { "pct": 80.0, "dimension": "customer" },
              "action": "alert", "enabled": true },
            { "metric": "cost_usd", "window": "day", "threshold": 12.5, "action": "block", "enabled": true }
        ]))
        .unwrap();
        assert!(md.contains("80% of revenue"), "{md}");
        assert!(md.contains("$12.50"), "{md}");
        assert!(!md.contains("$0 "), "{md}");
    }
}
