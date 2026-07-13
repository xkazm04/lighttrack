//! `get_margin` — profit rollup (revenue − LLM cost) per customer or product, most-unprofitable first.

use serde_json::Value;

use crate::md::{commafy, f, money, opt_f, opt_s, pct, s, short_ts, u, Align, Table};

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
