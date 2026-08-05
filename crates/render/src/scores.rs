//! `list_scores` — recent LLM-as-judge verdicts, with mean and a score-trend sparkline.

use serde_json::Value;

use crate::md::{f, money, opt_b, opt_f, pass_glyph, s, short_ts, sparkline, trunc, Align, Table};

pub(crate) fn list(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_No scores._".to_string());
    }
    let mut t = Table::new(&[
        ("When", Align::Left),
        ("Rubric", Align::Left),
        ("Score", Align::Right),
        ("Pass", Align::Left),
        ("Judge", Align::Left),
        ("Cost", Align::Right),
    ]);
    let mut vals: Vec<f64> = Vec::new();
    for r in rows {
        let value = f(r, "value");
        let max = opt_f(r, "max").unwrap_or(1.0);
        vals.push(value);
        let score_cell = if (max - 1.0).abs() < 1e-9 {
            format!("{value:.2}")
        } else {
            format!("{value:.2}/{max:.0}")
        };
        t.row(vec![
            short_ts(s(r, "created_at")),
            trunc(s(r, "rubric"), 36),
            score_cell,
            pass_glyph(opt_b(r, "pass")).to_string(),
            trunc(s(r, "scored_by"), 22),
            opt_f(r, "cost_usd")
                .map(money)
                .unwrap_or_else(|| "—".into()),
        ]);
    }
    let mean = vals.iter().sum::<f64>() / vals.len() as f64;
    // Scores arrive newest-first; reverse so the sparkline reads left=oldest → right=newest.
    let mut trend = vals.clone();
    trend.reverse();
    Some(format!(
        "**{} score(s)** · mean {:.2} · trend `{}`\n\n{}",
        rows.len(),
        mean,
        sparkline(&trend),
        t.render()
    ))
}
