//! `get_margin` — profit rollup (revenue − LLM cost) per customer or product, most-unprofitable first.

use serde_json::Value;

use crate::md::{commafy, f, money, opt_f, opt_s, pct, s, short_ts, sparkline, u, Align, Table};

pub(crate) fn report(v: &Value) -> Option<String> {
    let rows = v.get("rows")?.as_array()?;
    let dim = s(v, "dimension");
    let label = if dim == "product" { "Product" } else { "Customer" };
    let window = format!("{} → {}", short_ts(s(v, "since")), short_ts(s(v, "until")));
    if rows.is_empty() {
        return Some(format!(
            "### Margin by {dim} · {window}\n\n_No revenue or attributed cost in this window._"
        ));
    }

    let mut t = Table::new(&[
        (label, Align::Left),
        ("Revenue", Align::Right),
        ("LLM cost", Align::Right),
        ("Margin", Align::Right),
        ("Margin%", Align::Right),
        ("Calls", Align::Right),
    ]);
    for r in rows {
        let margin = f(r, "gross_margin_usd");
        let mpct = opt_f(r, "margin_pct");
        t.row(vec![
            format!("{} {}", glyph(margin, mpct), s(r, "key")),
            money(f(r, "revenue_usd")),
            money(f(r, "llm_cost_usd")),
            money(margin),
            mpct.map(pct).unwrap_or_else(|| "—".into()),
            commafy(u(r, "calls")),
        ]);
    }
    let mut out = format!(
        "### Margin by {dim} · {window}\n\n{}\n**Total: {} revenue − {} cost = {} margin**\n",
        t.render(),
        money(f(v, "total_revenue_usd")),
        money(f(v, "total_cost_usd")),
        money(f(v, "total_margin_usd")),
    );
    if let Some(note) = opt_s(v, "currency_note") {
        out.push_str(&format!("\n> ⚠️ {note}\n"));
    }
    Some(out)
}

/// `get_margin_trend` — a compact per-key margin sparkline table plus window totals.
pub(crate) fn trend(v: &Value) -> Option<String> {
    let dim = s(v, "dimension");
    let label = if dim == "product" { "Product" } else { "Customer" };
    let days = u(v, "days");
    let series = v.get("series")?.as_array()?;
    let totals = v.get("totals");

    let mut out = format!("### Margin trend by {dim} · last {days}d\n\n");
    if let Some(t) = totals {
        out.push_str(&format!(
            "**All keys:** {} revenue − {} cost = {} margin  ·  margin `{}`\n\n",
            money(f(t, "total_revenue_usd")),
            money(f(t, "total_cost_usd")),
            money(f(t, "total_margin_usd")),
            margin_spark(t),
        ));
    }
    if series.is_empty() {
        out.push_str("_No revenue or attributed cost in this window._");
        return Some(out);
    }

    let mut tbl = Table::new(&[
        (label, Align::Left),
        ("Margin trend", Align::Left),
        ("Revenue", Align::Right),
        ("Cost", Align::Right),
        ("Margin", Align::Right),
    ]);
    for row in series {
        let m = f(row, "total_margin_usd");
        tbl.row(vec![
            format!("{} {}", glyph(m, None), s(row, "key")),
            margin_spark(row),
            money(f(row, "total_revenue_usd")),
            money(f(row, "total_cost_usd")),
            money(m),
        ]);
    }
    let shown = series.len();
    let total_keys = u(v, "key_count");
    out.push_str(&tbl.render());
    if total_keys as usize > shown {
        out.push_str(&format!("\n_Showing top {shown} of {total_keys} by |margin|._\n"));
    }
    Some(out)
}

/// `get_margin_simulate` — pricing what-if: simulated vs actual margin per key, with the per-key delta.
pub(crate) fn simulate(v: &Value) -> Option<String> {
    let rows = v.get("rows")?.as_array()?;
    let dim = s(v, "dimension");
    let label = if dim == "product" { "Product" } else { "Customer" };
    let window = format!("{} → {}", short_ts(s(v, "since")), short_ts(s(v, "until")));

    let assumptions = v.get("assumptions");
    let ppm = assumptions.and_then(|a| opt_f(a, "price_per_mtok"));
    let flat = assumptions.and_then(|a| opt_f(a, "flat_monthly"));
    let days = assumptions.map(|a| f(a, "window_days")).unwrap_or(0.0);
    let mut model = Vec::new();
    if let Some(p) = ppm {
        model.push(format!("{}/Mtok", money(p)));
    }
    if let Some(fl) = flat {
        model.push(format!("{}/mo (prorated ×{:.2})", money(fl), days / 30.0));
    }
    let model = if model.is_empty() { "—".to_string() } else { model.join(" + ") };

    let mut out = format!("### Pricing what-if by {dim} · {window}\n\n_Simulated model: {model}. Read-only — nothing was written._\n\n");
    if rows.is_empty() {
        out.push_str("_No attributed usage or revenue in this window._");
        return Some(out);
    }

    let mut t = Table::new(&[
        (label, Align::Left),
        ("Tokens", Align::Right),
        ("LLM cost", Align::Right),
        ("Actual margin", Align::Right),
        ("Sim. margin", Align::Right),
        ("Δ", Align::Right),
    ]);
    for r in rows {
        let delta = f(r, "margin_delta_usd");
        t.row(vec![
            format!("{} {}", delta_glyph(delta), s(r, "key")),
            commafy(u(r, "tokens")),
            money(f(r, "llm_cost_usd")),
            money(f(r, "actual_margin_usd")),
            money(f(r, "simulated_margin_usd")),
            signed(delta),
        ]);
    }
    out.push_str(&t.render());
    out.push_str(&format!(
        "\n**Total: {} actual → {} simulated margin (Δ {})**\n",
        money(f(v, "total_actual_margin_usd")),
        money(f(v, "total_simulated_margin_usd")),
        signed(f(v, "total_margin_delta_usd")),
    ));
    if let Some(note) = opt_s(v, "currency_note") {
        out.push_str(&format!("\n> ⚠️ {note}\n"));
    }
    Some(out)
}

/// A signed money string with an explicit `+` for gains, so a what-if delta reads at a glance.
fn signed(x: f64) -> String {
    if x > 0.0 {
        format!("+{}", money(x))
    } else {
        money(x)
    }
}

/// 🟢 the hypothetical model improves margin · ⚪ neutral · 🔴 it earns less.
fn delta_glyph(delta: f64) -> &'static str {
    if delta > 0.0 {
        "🟢"
    } else if delta < 0.0 {
        "🔴"
    } else {
        "⚪"
    }
}

/// `get_customer_margin` — one customer's revenue/cost/margin headline plus cost split by model & name.
pub(crate) fn customer(v: &Value) -> Option<String> {
    let id = s(v, "customer_id");
    if id.is_empty() {
        return None;
    }
    let window = format!("{} → {}", short_ts(s(v, "since")), short_ts(s(v, "until")));
    let margin = f(v, "margin_usd");
    let mpct = opt_f(v, "margin_pct");
    let mut out = format!("### Customer `{id}` · {window}\n\n");
    out.push_str(&format!(
        "{} **{} revenue − {} cost = {} margin{}**\n\n",
        glyph(margin, mpct),
        money(f(v, "revenue_usd")),
        money(f(v, "cost_usd")),
        money(margin),
        mpct.map(|p| format!(" ({})", pct(p))).unwrap_or_default(),
    ));
    out.push_str(&breakdown_table("By model", v.get("by_model")));
    out.push_str(&breakdown_table("By use-case", v.get("by_name")));
    Some(out)
}

/// A cost breakdown sub-table (`key`, calls, cost), or an em-dash line when empty/absent.
fn breakdown_table(title: &str, rows: Option<&Value>) -> String {
    let rows = match rows.and_then(Value::as_array) {
        Some(r) if !r.is_empty() => r,
        _ => return format!("**{title}:** _none_\n\n"),
    };
    let mut t = Table::new(&[
        (title, Align::Left),
        ("Calls", Align::Right),
        ("Cost", Align::Right),
    ]);
    for r in rows {
        t.row(vec![
            s(r, "key").to_string(),
            commafy(u(r, "calls")),
            money(f(r, "cost_usd")),
        ]);
    }
    format!("{}\n", t.render())
}

/// A sparkline over a series' per-day `margin_usd`.
fn margin_spark(series: &Value) -> String {
    let pts = series.get("points").and_then(Value::as_array);
    let xs: Vec<f64> = pts
        .map(|a| a.iter().map(|p| f(p, "margin_usd")).collect())
        .unwrap_or_default();
    sparkline(&xs)
}

/// 🔴 losing money · ⚠️ thin margin (<20%) · 🟢 healthy.
fn glyph(margin: f64, pct: Option<f64>) -> &'static str {
    if margin < 0.0 {
        "🔴"
    } else if pct.is_some_and(|p| p < 0.2) {
        "⚠️"
    } else {
        "🟢"
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn simulate_renders_actual_and_simulated_columns_with_signed_delta() {
        let v = json!({
            "simulated": true,
            "dimension": "customer",
            "since": "2026-06-15T00:00:00Z",
            "until": "2026-07-15T00:00:00Z",
            "assumptions": { "price_per_mtok": 8.0, "flat_monthly": 5.0, "window_days": 30.0 },
            "total_actual_margin_usd": 15.5,
            "total_simulated_margin_usd": 37.5,
            "total_margin_delta_usd": 22.0,
            "rows": [
                { "key": "beta", "tokens": 1000000, "calls": 1, "llm_cost_usd": 1.5,
                  "actual_revenue_usd": 0.0, "actual_margin_usd": -1.5,
                  "simulated_revenue_usd": 13.0, "simulated_margin_usd": 11.5, "margin_delta_usd": 13.0 }
            ]
        });
        let out = super::simulate(&v).expect("renders");
        assert!(out.contains("Actual margin") && out.contains("Sim. margin"), "both columns present");
        assert!(out.contains("+$13.00"), "positive delta gets an explicit + sign");
        assert!(out.contains("$8.00/Mtok"), "echoes the token rate");
        assert!(out.contains("Read-only"), "flags the what-if as non-persisting");
    }

    #[test]
    fn simulate_empty_rows_is_graceful() {
        let v = json!({
            "simulated": true, "dimension": "product",
            "since": "2026-06-15T00:00:00Z", "until": "2026-07-15T00:00:00Z",
            "assumptions": { "flat_monthly": 9.0, "window_days": 30.0 },
            "total_actual_margin_usd": 0.0, "total_simulated_margin_usd": 0.0,
            "total_margin_delta_usd": 0.0, "rows": []
        });
        let out = super::simulate(&v).expect("renders");
        assert!(out.contains("No attributed usage or revenue"));
    }
}
