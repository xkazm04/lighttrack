//! `list_prices` — the DB-backed model price book (per-million-token rates).

use serde_json::Value;

use crate::md::{opt_f, opt_s, rate, s, trunc, Align, Table};

pub(crate) fn list(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_Empty price book._".to_string());
    }
    let mut sorted: Vec<&Value> = rows.iter().collect();
    sorted.sort_by(|a, b| {
        s(a, "provider")
            .cmp(s(b, "provider"))
            .then_with(|| s(a, "model").cmp(s(b, "model")))
    });

    let mut t = Table::new(&[
        ("Provider", Align::Left),
        ("Model", Align::Left),
        ("In $/Mtok", Align::Right),
        ("Out $/Mtok", Align::Right),
        ("Cached", Align::Right),
        ("Source", Align::Left),
    ]);
    for r in &sorted {
        let cached = opt_f(r, "cached_input_per_mtok")
            .map(rate)
            .unwrap_or_else(|| "—".into());
        let src = match opt_s(r, "source_url").filter(|x| !x.is_empty()) {
            Some(u) => trunc(host(u), 24),
            None => "—".into(),
        };
        t.row(vec![
            s(r, "provider").to_string(),
            trunc(s(r, "model"), 30),
            rate(opt_f(r, "input_per_mtok").unwrap_or(0.0)),
            rate(opt_f(r, "output_per_mtok").unwrap_or(0.0)),
            cached,
            src,
        ]);
    }
    Some(t.render())
}

fn host(url: &str) -> &str {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(url)
}
