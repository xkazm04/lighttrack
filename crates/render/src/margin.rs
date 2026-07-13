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
