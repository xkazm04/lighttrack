//! `collective` — the cross-instance model leaderboard + a single instance's contributable digest.
//! Shared so the CLI and MCP render the same tables the network is built around.
//!
//! Leaderboard input: `{ contributors, n_models, task_type?, rows: [ {provider, model, task_type,
//! quality, pass_rate, avg_cost_usd, p50_latency_ms?, n_contributors, n_runs, n_cases} ] }`.
//! Digest input:      `{ schema_version, contributor_id, min_cases, entries: [ {provider, model,
//! task_type, quality, pass_rate, avg_cost_usd, p50_latency_ms?, n_runs, n_cases} ] }`.

use serde_json::Value;

use crate::md::{commafy, f, money, opt_u, pct, s, u, Align, Table};

/// The merged public leaderboard (highest quality first).
pub(crate) fn leaderboard(v: &Value) -> Option<String> {
    let rows = v.get("rows")?.as_array()?;
    let contributors = u(v, "contributors");
    if rows.is_empty() {
        return Some(format!(
            "_No collective data yet ({contributors} contributor(s))._ Contribute with `lt collective contribute --hub <url>`."
        ));
    }
    let mut t = model_table(true);
    for r in rows {
        t.row(model_row(r, true));
    }
    let scope = v
        .get("task_type")
        .and_then(Value::as_str)
        .map(|tt| format!(" · task={tt}"))
        .unwrap_or_default();
    Some(format!(
        "### Collective model leaderboard — {} model(s), {contributors} contributor(s){scope}\n\n{}",
        rows.len(),
        t.render()
    ))
}

/// This instance's privacy-safe digest — what it would contribute to a hub.
pub(crate) fn digest(v: &Value) -> Option<String> {
    let entries = v.get("entries")?.as_array()?;
    let contributor = s(v, "contributor_id");
    let min_cases = u(v, "min_cases");
    if entries.is_empty() {
        return Some(format!(
            "_No publishable buckets: every (model, task) has < {min_cases} cases (k-anonymity floor)._"
        ));
    }
    let mut t = model_table(false);
    for e in entries {
        t.row(model_row(e, false));
    }
    Some(format!(
        "### Contributable digest — {} bucket(s), as `{contributor}` (k≥{min_cases})\n\n{}",
        entries.len(),
        t.render()
    ))
}

/// Shared columns; the leaderboard has a `Sources` column, the digest does not.
fn model_table(with_sources: bool) -> Table {
    let mut cols = vec![
        ("Provider", Align::Left),
        ("Model", Align::Left),
        ("Task", Align::Left),
        ("Quality", Align::Right),
        ("Pass%", Align::Right),
        ("Cost/case", Align::Right),
        ("p50", Align::Right),
    ];
    if with_sources {
        cols.push(("Sources", Align::Right));
    }
    cols.push(("Runs", Align::Right));
    cols.push(("Cases", Align::Right));
    Table::new(&cols)
}

fn model_row(r: &Value, with_sources: bool) -> Vec<String> {
    let mut cells = vec![
        s(r, "provider").to_string(),
        s(r, "model").to_string(),
        s(r, "task_type").to_string(),
        format!("{:.3}", f(r, "quality")),
        pct(f(r, "pass_rate")),
        money(f(r, "avg_cost_usd")),
        opt_u(r, "p50_latency_ms").map(|m| format!("{m}ms")).unwrap_or_else(|| "—".into()),
    ];
    if with_sources {
        cells.push(u(r, "n_contributors").to_string());
    }
    cells.push(commafy(u(r, "n_runs")));
    cells.push(commafy(u(r, "n_cases")));
    cells
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn leaderboard_renders_table_and_header() {
        let v = json!({
            "contributors": 3, "n_models": 1, "rows": [
                {"provider":"anthropic","model":"haiku","task_type":"qa","quality":0.87,
                 "pass_rate":0.9,"avg_cost_usd":0.0038,"p50_latency_ms":820,
                 "n_contributors":3,"n_runs":12,"n_cases":1200}
            ]
        });
        let md = leaderboard(&v).unwrap();
        assert!(md.contains("Collective model leaderboard"));
        assert!(md.contains("haiku"));
        assert!(md.contains("0.870"));
        assert!(md.contains("820ms"));
        assert!(md.contains("1,200"));
    }

    #[test]
    fn empty_leaderboard_nudges_contribution() {
        let md = leaderboard(&json!({"contributors":0,"rows":[]})).unwrap();
        assert!(md.contains("No collective data"));
        assert!(md.contains("contribute"));
    }

    #[test]
    fn empty_digest_explains_k_anonymity() {
        let md = digest(&json!({"contributor_id":"anonymous","min_cases":5,"entries":[]})).unwrap();
        assert!(md.contains("k-anonymity"));
    }
}
