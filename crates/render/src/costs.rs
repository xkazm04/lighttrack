//! `get_cost_summary` — usage rollup grouped by project + provider + model, sorted by spend.
//! `get_usecases` — the same, grouped by use-case (`name`, falling back to model) × provider × model.

use serde_json::Value;

use crate::md::{commafy, f, money, opt_s, s, u, Align, Table};

pub(crate) fn summary(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_No usage recorded yet._".to_string());
    }
    let mut sorted: Vec<&Value> = rows.iter().collect();
    sorted.sort_by(|a, b| f(b, "cost_usd").total_cmp(&f(a, "cost_usd")));

    let mut t = Table::new(&[
        ("Project", Align::Left),
        ("Provider", Align::Left),
        ("Model", Align::Left),
        ("Calls", Align::Right),
        ("In tok", Align::Right),
        ("Out tok", Align::Right),
        ("Cost", Align::Right),
    ]);
    let (mut calls, mut in_t, mut out_t, mut cost) = (0u64, 0u64, 0u64, 0.0f64);
    for r in &sorted {
        let c = u(r, "calls");
        let i = u(r, "input_tokens");
        let o = u(r, "output_tokens");
        let cu = f(r, "cost_usd");
        calls += c;
        in_t += i;
        out_t += o;
        cost += cu;
        t.row(vec![
            s(r, "project_id").to_string(),
            s(r, "provider").to_string(),
            s(r, "model").to_string(),
            commafy(c),
            commafy(i),
            commafy(o),
            money(cu),
        ]);
    }
    Some(format!(
        "{}\n**Total: {} across {} calls** ({} in / {} out tokens)\n",
        t.render(),
        money(cost),
        commafy(calls),
        commafy(in_t),
        commafy(out_t),
    ))
}

/// Use-case cost rollup: usage + cost per (use-case, provider, model), most expensive first. An
/// unnamed call's use-case shows as its model (mirroring how the API buckets it).
pub(crate) fn usecases(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_No use-case usage recorded in this window._".to_string());
    }
    let mut sorted: Vec<&Value> = rows.iter().collect();
    sorted.sort_by(|a, b| f(b, "cost_usd").total_cmp(&f(a, "cost_usd")));

    let mut t = Table::new(&[
        ("Use-case", Align::Left),
        ("Provider", Align::Left),
        ("Model", Align::Left),
        ("Calls", Align::Right),
        ("In tok", Align::Right),
        ("Out tok", Align::Right),
        ("Cost", Align::Right),
    ]);
    let (mut calls, mut cost) = (0u64, 0.0f64);
    for r in &sorted {
        let c = u(r, "calls");
        let cu = f(r, "cost_usd");
        calls += c;
        cost += cu;
        // `name` is optional; fall back to the model so every row reads as a real use-case.
        let usecase = opt_s(r, "name")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| s(r, "model"));
        t.row(vec![
            usecase.to_string(),
            s(r, "provider").to_string(),
            s(r, "model").to_string(),
            commafy(c),
            commafy(u(r, "input_tokens")),
            commafy(u(r, "output_tokens")),
            money(cu),
        ]);
    }
    Some(format!(
        "### Use-case cost rollup — {} bucket(s)\n\n{}\n**Total: {} across {} calls**\n",
        sorted.len(),
        t.render(),
        money(cost),
        commafy(calls),
    ))
}
