//! `get_forecast` — the predictive cost/margin surface: a projected-spend headline, a per-budget
//! breach-ETA table, a per-customer/product margin-erosion table, and the pre-emptive alerts.
//!
//! Input shape (see `crates/api/src/forecast.rs::ForecastResponse`):
//! `{ project_id, dimension, horizon_days, lookback_days, spend: {projected_daily_cost_usd,
//!    projected_cost_next_7d_usd, projected_cost_next_30d_usd}, budgets: [{rule_id, metric, window,
//!    threshold, current, projected_daily, eta_days?}], margins: [{key, revenue_per_day, cost_per_day,
//!    margin_usd, currently_profitable, eta_unprofitable_days?}], alerts: [{severity, message}] }`.

use serde_json::Value;

use crate::md::{f, money, opt_f, s, Align, Table};

pub(crate) fn report(v: &Value) -> Option<String> {
    let project = s(v, "project_id");
    let dim = s(v, "dimension");
    let horizon = v.get("horizon_days").and_then(Value::as_u64).unwrap_or(0);
    let lookback = v.get("lookback_days").and_then(Value::as_u64).unwrap_or(0);
    let spend = v.get("spend")?;

    let mut out = format!(
        "### Forecast — `{project}` (by {dim}, {horizon}d ahead from {lookback}d history)\n\n\
         **Projected spend:** {}/day · {} next 7d · {} next 30d\n",
        money(f(spend, "projected_daily_cost_usd")),
        money(f(spend, "projected_cost_next_7d_usd")),
        money(f(spend, "projected_cost_next_30d_usd")),
    );

    if let Some(t) = budgets_table(v) {
        out.push('\n');
        out.push_str(&t);
    }
    if let Some(t) = margins_table(v) {
        out.push('\n');
        out.push_str(&t);
    }
    if let Some(a) = alerts_block(v) {
        out.push('\n');
        out.push_str(&a);
    }
    Some(out)
}

/// A human ETA: "breached" when 0, "~N days" otherwise, "—" when the budget isn't on track to breach.
fn eta(days: Option<f64>) -> String {
    match days {
        Some(d) if d <= 0.0 => "now".to_string(),
        Some(d) => format!("~{d:.0}d"),
        None => "—".to_string(),
    }
}

/// Threshold/current render depend on the metric: cost is money, calls/tokens are counts.
fn amount(metric: &str, x: f64) -> String {
    if metric == "cost_usd" {
        money(x)
    } else {
        crate::md::commafy(x as u64)
    }
}

fn budgets_table(v: &Value) -> Option<String> {
    let rows = v.get("budgets")?.as_array()?;
    if rows.is_empty() {
        return None;
    }
    let mut t = Table::new(&[
        ("Budget rule", Align::Left),
        ("Metric", Align::Left),
        ("Window", Align::Left),
        ("Current", Align::Right),
        ("Threshold", Align::Right),
        ("Proj/day", Align::Right),
        ("Breach ETA", Align::Right),
    ]);
    for r in rows {
        let metric = s(r, "metric");
        t.row(vec![
            s(r, "rule_id").to_string(),
            metric.to_string(),
            s(r, "window").to_string(),
            amount(metric, f(r, "current")),
            amount(metric, f(r, "threshold")),
            amount(metric, f(r, "projected_daily")),
            eta(opt_f(r, "eta_days")),
        ]);
    }
    Some(format!("**Budgets**\n\n{}\n", t.render()))
}

fn margins_table(v: &Value) -> Option<String> {
    let rows = v.get("margins")?.as_array()?;
    if rows.is_empty() {
        return None;
    }
    let mut t = Table::new(&[
        ("Key", Align::Left),
        ("Rev/day", Align::Right),
        ("Cost/day", Align::Right),
        ("Margin", Align::Right),
        ("Turns unprofitable", Align::Right),
    ]);
    for r in rows {
        let profitable = r.get("currently_profitable").and_then(Value::as_bool).unwrap_or(false);
        let turns = if profitable {
            eta(opt_f(r, "eta_unprofitable_days"))
        } else {
            "already ❌".to_string()
        };
        t.row(vec![
            s(r, "key").to_string(),
            money(f(r, "revenue_per_day")),
            money(f(r, "cost_per_day")),
            money(f(r, "margin_usd")),
            turns,
        ]);
    }
    Some(format!("**Margins** (worst first)\n\n{}\n", t.render()))
}

fn alerts_block(v: &Value) -> Option<String> {
    let rows = v.get("alerts")?.as_array()?;
    if rows.is_empty() {
        return None;
    }
    let mut out = String::from("**Pre-emptive alerts**\n\n");
    for a in rows {
        let glyph = if s(a, "severity") == "high" { "🔴" } else { "🟡" };
        out.push_str(&format!("- {glyph} {}\n", s(a, "message")));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "project_id": "p1", "dimension": "customer", "horizon_days": 14, "lookback_days": 14,
            "spend": {
                "projected_daily_cost_usd": 1.5, "projected_cost_next_7d_usd": 10.5,
                "projected_cost_next_30d_usd": 45.0
            },
            "budgets": [{
                "rule_id": "r1", "metric": "cost_usd", "window": "day", "threshold": 20.0,
                "current": 12.0, "projected_daily": 1.5, "eta_days": 5.3
            }],
            "margins": [{
                "key": "acme", "revenue_per_day": 3.0, "cost_per_day": 2.5, "margin_usd": 15.0,
                "currently_profitable": true, "eta_unprofitable_days": 9.0
            }],
            "alerts": [{ "kind": "budget_breach", "severity": "warning", "subject": "r1",
                         "eta_days": 5.3, "message": "on track to breach in about 5 days" }]
        })
    }

    #[test]
    fn report_renders_headline_and_all_sections() {
        let md = report(&sample()).unwrap();
        assert!(md.contains("Forecast — `p1`"));
        assert!(md.contains("$1.50/day"));
        assert!(md.contains("Budgets"));
        assert!(md.contains("~5d"));
        assert!(md.contains("Margins"));
        assert!(md.contains("acme"));
        assert!(md.contains("Pre-emptive alerts"));
        assert!(md.contains("on track to breach"));
    }

    #[test]
    fn margin_already_unprofitable_is_flagged() {
        let mut v = sample();
        v["margins"][0]["currently_profitable"] = json!(false);
        let md = report(&v).unwrap();
        assert!(md.contains("already ❌"));
    }

    #[test]
    fn sections_absent_when_empty() {
        let mut v = sample();
        v["budgets"] = json!([]);
        v["margins"] = json!([]);
        v["alerts"] = json!([]);
        let md = report(&v).unwrap();
        assert!(!md.contains("Budgets"));
        assert!(!md.contains("Margins"));
        assert!(!md.contains("Pre-emptive alerts"));
    }
}
