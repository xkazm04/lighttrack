//! `collective` — the cross-instance model leaderboard + a single instance's contributable digest.
//! Shared so the CLI and MCP render the same tables the network is built around.
//!
//! Leaderboard input: `{ contributors, n_models, n_rows, task_type?, rows: [ {provider, model, task_type,
//! quality, quality_ci95?, pass_rate, avg_cost_usd, p50_latency_ms?, p95_latency_ms?, low_confidence,
//! judge_providers?, mixed_judges?, n_contributors, n_runs, n_cases} ] }`.
//! Digest input:      `{ schema_version, contributor_id, min_cases, entries: [ {provider, model,
//! task_type, quality, pass_rate, avg_cost_usd, p50_latency_ms?, p95_latency_ms?, quality_variance?,
//! judge_provider?, rubric_fingerprint?, n_runs, n_cases} ] }`.

use serde_json::Value;

use crate::md::{commafy, f, money, opt_f, opt_u, pct, s, u, Align, Table};

/// Column flags: the leaderboard carries a `Sources` count and a merged 95% CI; the digest does not.
struct Cols {
    sources: bool,
    ci: bool,
}

/// The merged public leaderboard (highest quality first).
pub(crate) fn leaderboard(v: &Value) -> Option<String> {
    let rows = v.get("rows")?.as_array()?;
    let contributors = u(v, "contributors");
    if rows.is_empty() {
        return Some(format!(
            "_No collective data yet ({contributors} contributor(s))._ Contribute with `lt collective contribute --hub <url>`."
        ));
    }
    let cols = Cols { sources: true, ci: true };
    let mut t = model_table(&cols);
    let mut any_low = false;
    for r in rows {
        any_low |= r.get("low_confidence").and_then(Value::as_bool).unwrap_or(false);
        t.row(model_row(r, &cols));
    }
    let scope = v
        .get("task_type")
        .and_then(Value::as_str)
        .map(|tt| format!(" · task={tt}"))
        .unwrap_or_default();
    // Honest footnotes: what the annotations mean.
    let mut notes = vec![
        "p50 is an approximate case-weighted mean of contributors' medians; p95 is the worst observed.",
        "±95% is an approximate CI on quality; `n/a` = insufficient variance data across contributors.",
        "Confidence = total cases × contributing sources backing the row.",
    ];
    if any_low {
        notes.push("† low-confidence row: too few total cases to rank authoritatively.");
    }
    Some(format!(
        "### Collective model leaderboard — {} model(s), {contributors} contributor(s){scope}\n\n{}\n\n_{}_",
        rows.len(),
        t.render(),
        notes.join(" ")
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
    let cols = Cols { sources: false, ci: false };
    let mut t = model_table(&cols);
    for e in entries {
        t.row(model_row(e, &cols));
    }
    Some(format!(
        "### Contributable digest — {} bucket(s), as `{contributor}` (k≥{min_cases})\n\n{}",
        entries.len(),
        t.render()
    ))
}

fn model_table(cols: &Cols) -> Table {
    let mut c = vec![
        ("Provider", Align::Left),
        ("Model", Align::Left),
        ("Task", Align::Left),
        ("Quality", Align::Right),
    ];
    if cols.ci {
        c.push(("±95%", Align::Right));
    }
    c.push(("Pass%", Align::Right));
    c.push(("Cost/case", Align::Right));
    c.push(("p50", Align::Right));
    c.push(("p95", Align::Right));
    c.push(("Judge", Align::Left));
    c.push(("Runs", Align::Right));
    if cols.sources {
        // Leaderboard: a single confidence column folding total cases × contributing sources, so a
        // reader sees at a glance how much evidence backs the row (paired with the † low-confidence flag).
        c.push(("Confidence", Align::Right));
    } else {
        c.push(("Cases", Align::Right));
    }
    Table::new(&c)
}

fn model_row(r: &Value, cols: &Cols) -> Vec<String> {
    // A low-confidence leaderboard row is flagged with a trailing † in the Confidence column.
    let low = r.get("low_confidence").and_then(Value::as_bool).unwrap_or(false);
    let mut cells = vec![
        s(r, "provider").to_string(),
        s(r, "model").to_string(),
        s(r, "task_type").to_string(),
        format!("{:.3}", f(r, "quality")),
    ];
    if cols.ci {
        cells.push(
            opt_f(r, "quality_ci95").map(|c| format!("±{c:.3}")).unwrap_or_else(|| "n/a".into()),
        );
    }
    cells.push(pct(f(r, "pass_rate")));
    cells.push(money(f(r, "avg_cost_usd")));
    cells.push(lat(r, "p50_latency_ms"));
    cells.push(lat(r, "p95_latency_ms"));
    cells.push(judge_cell(r, cols));
    cells.push(commafy(u(r, "n_runs")));
    if cols.sources {
        cells.push(confidence_cell(r, low));
    } else {
        cells.push(commafy(u(r, "n_cases")));
    }
    cells
}

/// The leaderboard confidence cell: `{cases} × {sources}` — total cases backing the row over the number
/// of distinct contributing instances — with a trailing `†` mirroring the `low_confidence` flag.
fn confidence_cell(r: &Value, low: bool) -> String {
    let cell = format!("{} × {}", commafy(u(r, "n_cases")), u(r, "n_contributors"));
    if low {
        format!("{cell} †")
    } else {
        cell
    }
}

fn lat(r: &Value, key: &str) -> String {
    opt_u(r, key).map(|m| format!("{m}ms")).unwrap_or_else(|| "—".into())
}

/// The judge cell: on the leaderboard, the distinct judge families (or `mixed(n)` when they disagree);
/// on the digest, the single coarse judge family for the bucket.
fn judge_cell(r: &Value, cols: &Cols) -> String {
    if cols.sources {
        let js: Vec<&str> =
            r.get("judge_providers").and_then(Value::as_array).map(|a| {
                a.iter().filter_map(Value::as_str).collect()
            }).unwrap_or_default();
        match js.len() {
            0 => "—".into(),
            1 => js[0].to_string(),
            n => format!("mixed({n})"),
        }
    } else {
        r.get("judge_provider").and_then(Value::as_str).unwrap_or("—").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn leaderboard_renders_ci_p95_and_low_confidence() {
        let v = json!({
            "contributors": 3, "n_models": 2, "rows": [
                {"provider":"anthropic","model":"haiku","task_type":"qa","quality":0.87,
                 "quality_ci95":0.028,"pass_rate":0.9,"avg_cost_usd":0.0038,
                 "p50_latency_ms":820,"p95_latency_ms":2100,"low_confidence":false,
                 "judge_providers":["anthropic","openai"],"mixed_judges":2,
                 "n_contributors":3,"n_runs":12,"n_cases":1200},
                {"provider":"openai","model":"gpt-x","task_type":"qa","quality":0.80,
                 "pass_rate":0.8,"avg_cost_usd":0.002,"p50_latency_ms":600,
                 "low_confidence":true,"judge_providers":["google"],
                 "n_contributors":1,"n_runs":1,"n_cases":12}
            ]
        });
        let md = leaderboard(&v).unwrap();
        assert!(md.contains("Collective model leaderboard"));
        assert!(md.contains("0.870"));
        assert!(md.contains("±0.028"), "CI half-width surfaced");
        assert!(md.contains("2100ms"), "p95 surfaced");
        assert!(md.contains("n/a"), "missing CI shown as n/a (insufficient variance)");
        assert!(md.contains("Confidence"), "confidence column present");
        assert!(md.contains("1,200 × 3"), "confidence = cases × sources");
        assert!(md.contains("12 × 1 †"), "low-confidence row flagged in the confidence column");
        assert!(md.contains("Confidence = total cases"), "legend explains the confidence column");
        assert!(md.contains("low-confidence row"), "legend explains the dagger");
        assert!(md.contains("mixed(2)"), "mixed judges surfaced");
        assert!(md.contains("google"), "single judge family surfaced");
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

    #[test]
    fn digest_renders_p95_without_ci_or_sources() {
        let v = json!({"contributor_id":"c-abc","min_cases":5,"entries":[
            {"provider":"anthropic","model":"haiku","task_type":"qa","quality":0.87,
             "pass_rate":0.9,"avg_cost_usd":0.0038,"p50_latency_ms":820,"p95_latency_ms":1500,
             "judge_provider":"openai","n_runs":3,"n_cases":300}
        ]});
        let md = digest(&v).unwrap();
        assert!(md.contains("1500ms"), "digest shows p95");
        assert!(md.contains("openai"), "digest shows the single judge family");
        assert!(!md.contains("±95%"), "digest has no CI column");
        assert!(!md.contains("Sources"), "digest has no Sources column");
    }
}
