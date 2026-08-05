//! `compare` — the runner's multi-target benchmark leaderboard (quality × cost × latency). Shared so
//! `lt-runner bench` (compare mode), the CLI, and MCP all emit the same table instead of a bespoke one.
//!
//! Input shape (built by the runner): `{ "n_cases": N, "targets": [ {label, mean, pass_rate,
//! agreement, gen_cost_usd, judge_cost_usd, p50_latency_ms, errored} ], "best": {…} }`.
//!
//! `best` — when the caller supplies it — carries the runner's *tested* superiority claim. This
//! layer never re-derives statistics (there is one statistics path, in the runner); it only refuses
//! to print a stronger sentence than the claim it was given.

use serde_json::Value;

use crate::md::{f, money, opt_u, pct, s, u, Align, Table};

/// The winner line. With a tested claim we say "Best" only when the separation is real, and name the
/// correction; without one we say "Highest mean" — true of the sample, and not a claim about models.
fn winner_line(best: Option<&Value>, fallback: Option<(&str, f64)>) -> Option<String> {
    let Some(b) = best.filter(|b| b.is_object()) else {
        let (label, mean) = fallback?;
        return Some(format!(
            "\n**Highest mean: {label} ({mean:.2})** — not tested for significance.\n"
        ));
    };
    let label = s(b, "label");
    let mean = f(b, "mean");
    let correction = b
        .get("correction")
        .and_then(Value::as_str)
        .unwrap_or("uncorrected");
    let p = b.get("p_value").and_then(Value::as_f64);
    if b.get("significant").and_then(Value::as_bool) == Some(true) {
        let runner_up = s(b, "runner_up");
        let p_txt = p.map(|p| format!(", p={p:.4}")).unwrap_or_default();
        return Some(format!(
            "\n**Best: {label} ({mean:.2})** — significantly ahead of {runner_up}{p_txt}; \
             {correction}.\n"
        ));
    }
    let note = b
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or("no significant difference from the runner-up");
    let p_txt = p
        .map(|p| format!(" (p={p:.4}; {correction})"))
        .unwrap_or_default();
    Some(format!(
        "\nHighest mean: {label} ({mean:.2}) — {note}{p_txt}.\n"
    ))
}

pub(crate) fn leaderboard(v: &Value) -> Option<String> {
    let targets = v.get("targets")?.as_array()?;
    if targets.is_empty() {
        return Some("_No comparison targets._".to_string());
    }
    let n_cases = v.get("n_cases").and_then(Value::as_u64).unwrap_or(0);

    let mut t = Table::new(&[
        ("Target", Align::Left),
        ("Mean", Align::Right),
        ("Pass%", Align::Right),
        ("Agree", Align::Right),
        ("Gen$", Align::Right),
        ("Judge$", Align::Right),
        ("p50", Align::Right),
        ("Err", Align::Right),
    ]);
    // Best = highest mean among targets that didn't error out every case (mirrors the runner's rule).
    let mut best: Option<(&str, f64)> = None;
    for r in targets {
        let label = s(r, "label");
        let mean = f(r, "mean");
        let errored = u(r, "errored");
        if errored < n_cases && best.is_none_or(|(_, bm)| mean > bm) {
            best = Some((label, mean));
        }
        t.row(vec![
            label.to_string(),
            format!("{mean:.2}"),
            pct(f(r, "pass_rate")),
            format!("{:.2}", f(r, "agreement")),
            money(f(r, "gen_cost_usd")),
            money(f(r, "judge_cost_usd")),
            opt_u(r, "p50_latency_ms")
                .map(|m| format!("{m}ms"))
                .unwrap_or_else(|| "—".into()),
            errored.to_string(),
        ]);
    }
    let mut out = format!("### Comparison — {n_cases} case(s)\n\n{}", t.render());
    // A cost-halted comparison is announced ABOVE the winner line: the table is over whatever cases
    // the money reached, so the ranking must not be read as a finished result.
    if v.get("budget_halted").and_then(Value::as_bool) == Some(true) {
        let spent = f(v, "spend_usd");
        let limit = v.get("budget_limit_usd").and_then(Value::as_f64);
        let cap = limit
            .map(|l| format!(" (limit {})", money(l)))
            .unwrap_or_default();
        out.push_str(&format!(
            "\n**PARTIAL — the run was halted at its spend ceiling after {}{cap}; some cases were \
             never judged.**\n",
            money(spent)
        ));
    }
    // Unpriced models make every $ column a lower bound. Surfaced here rather than only inside each
    // run report's nested array, which nobody reading the table ever opens.
    if let Some(w) = v
        .get("price_warnings")
        .and_then(Value::as_array)
        .filter(|w| !w.is_empty())
    {
        let models: Vec<&str> = w.iter().filter_map(Value::as_str).collect();
        out.push_str(&format!(
            "\n_No price book entry for {} — the $ columns are a lower bound._\n",
            models.join(", ")
        ));
    }
    if let Some(line) = winner_line(v.get("best"), best) {
        out.push_str(&line);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{leaderboard, winner_line};
    use serde_json::json;

    #[test]
    fn a_halted_run_says_partial_and_names_the_unpriced_models() {
        let md = leaderboard(&json!({
            "n_cases": 10,
            "targets": [{ "label": "a", "mean": 0.9, "errored": 0 }],
            "budget_halted": true, "spend_usd": 12.5, "budget_limit_usd": 12.0,
            "price_warnings": ["zz/yy"],
        }))
        .unwrap();
        assert!(
            md.contains("**PARTIAL"),
            "a halted run must not read as a finished one"
        );
        assert!(
            md.contains("$12.50") && md.contains("$12.00"),
            "spend and ceiling are both shown"
        );
        assert!(md.contains("zz/yy") && md.contains("lower bound"));
    }

    #[test]
    fn a_complete_run_carries_no_partial_banner() {
        let md = leaderboard(&json!({
            "n_cases": 10, "targets": [{ "label": "a", "mean": 0.9, "errored": 0 }],
            "budget_halted": false, "price_warnings": [],
        }))
        .unwrap();
        assert!(!md.contains("PARTIAL") && !md.contains("lower bound"));
    }

    #[test]
    fn a_tested_win_is_the_only_thing_called_best() {
        let claim = json!({
            "label": "gpt-4o", "mean": 0.91, "significant": true, "runner_up": "haiku",
            "p_value": 0.0012, "correction": "Bonferroni over 3 target pair(s), family-wise α=0.05",
        });
        let line = winner_line(Some(&claim), None).unwrap();
        assert!(line.contains("**Best: gpt-4o (0.91)**"));
        assert!(line.contains("significantly ahead of haiku"));
        assert!(
            line.contains("p=0.0012") && line.contains("Bonferroni"),
            "the method is named"
        );
    }

    #[test]
    fn an_untested_gap_is_only_the_highest_mean() {
        // The evidence case: 0.01 apart, overlapping intervals — no bold winner.
        let claim = json!({
            "label": "a", "mean": 0.87, "significant": false, "runner_up": "b",
            "runner_up_mean": 0.86, "p_value": 0.62,
            "note": "no significant difference from the runner-up at the corrected α",
            "correction": "Bonferroni over 1 target pair(s), family-wise α=0.05",
        });
        let line = winner_line(Some(&claim), None).unwrap();
        assert!(
            !line.contains("**Best"),
            "an undecidable ranking must not be bolded as a winner"
        );
        assert!(line.contains("Highest mean: a (0.87)"));
        assert!(line.contains("no significant difference"));
    }

    #[test]
    fn without_a_claim_the_argmax_is_labelled_as_untested() {
        let line = winner_line(None, Some(("solo", 0.5))).unwrap();
        assert!(line.contains("**Highest mean: solo (0.50)**"));
        assert!(line.contains("not tested for significance"));
        // Nothing at all to say → no line rather than an empty claim.
        assert!(winner_line(None, None).is_none());
    }
}
