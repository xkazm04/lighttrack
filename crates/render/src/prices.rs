//! `list_prices` — the DB-backed model price book (per-million-token rates).

use serde_json::Value;

use crate::md::{opt_f, opt_s, rate, s, short_ts, trunc, Align, Table};

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
        // The book is dated now (M26): a rate without the date it took effect, and without the day
        // somebody last checked it, is a number you cannot audit.
        ("Effective", Align::Left),
        ("Verified", Align::Left),
        ("Source", Align::Left),
    ]);
    for r in &sorted {
        t.row(cells(r));
    }
    Some(t.render())
}

/// One key's price timeline, newest first — `GET /v1/prices/history/:provider/:model`.
pub(crate) fn history(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_No stored rate for that model._".to_string());
    }
    let mut t = Table::new(&[
        ("Provider", Align::Left),
        ("Model", Align::Left),
        ("In $/Mtok", Align::Right),
        ("Out $/Mtok", Align::Right),
        ("Cached", Align::Right),
        ("Effective", Align::Left),
        ("Verified", Align::Left),
        ("Source", Align::Left),
    ]);
    // Server order is authoritative (newest first) — re-sorting here would hide a backend that
    // returned the timeline the wrong way round.
    for r in rows {
        t.row(cells(r));
    }
    let mut out = t.render();
    if let Some(note) = rows.iter().find_map(|r| opt_s(r, "note")) {
        out.push_str(&format!("\n\n_Latest note: {}_\n", trunc(note, 120)));
    }
    Some(out)
}

fn cells(r: &Value) -> Vec<String> {
    let dash = || "—".to_string();
    vec![
        s(r, "provider").to_string(),
        trunc(s(r, "model"), 30),
        rate(opt_f(r, "input_per_mtok").unwrap_or(0.0)),
        rate(opt_f(r, "output_per_mtok").unwrap_or(0.0)),
        opt_f(r, "cached_input_per_mtok")
            .map(rate)
            .unwrap_or_else(dash),
        opt_s(r, "effective_from")
            .or_else(|| opt_s(r, "effective_date"))
            .map(short_ts)
            .unwrap_or_else(dash),
        opt_s(r, "verified_at").map(short_ts).unwrap_or_else(dash),
        opt_s(r, "source_url")
            .filter(|x| !x.is_empty())
            .map(|u| trunc(host(u), 24))
            .unwrap_or_else(dash),
    ]
}

fn host(url: &str) -> &str {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(url)
}
