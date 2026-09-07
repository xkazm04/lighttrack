//! `compare` — the runner's multi-target benchmark leaderboard (quality × cost × latency). Shared so
//! `lt-runner bench` (compare mode), the CLI, and MCP all emit the same table instead of a bespoke one.
//!
//! Input shape (built by the runner): `{ "n_cases": N, "targets": [ {label, mean, pass_rate,
//! agreement, gen_cost_usd, judge_cost_usd, p50_latency_ms, errored} ], "best": {…} }`.
//!
//! `best` — when the caller supplies it — carries the runner's *tested* superiority claim, and
//! `frontier`/`recommendation` its cost–quality surface and cheapest-sufficient pick. This layer
//! never re-derives statistics (there is one statistics path, in the runner); it only refuses to
//! print a stronger sentence than the claim it was given. Every one of those keys is optional: a
//! stored table rendered by the CLI or MCP has none of them and keeps exactly the columns and lines
//! it has always had.

use serde_json::Value;

use crate::md::{f, money, opt_b, opt_f, opt_s, opt_u, pct, s, u, Align, Table};

/// The caveats a claim carries, as one trailing sentence — or nothing when it carries none.
///
/// Printed on **both** branches, and the significant one is why this exists. A `best` that rests on
/// a subset of the cases (the targets errored on different ones, so the paired test ran over their
/// intersection) is still a real, tested claim — and rendering it as a bare "significantly ahead"
/// would hide the one fact a reader needs to weigh it. This layer derives nothing: the runner
/// decides what the caveats are, and an empty list prints nothing at all.
fn caveat_txt(b: &Value) -> String {
    let cs: Vec<&str> = b
        .get("caveats")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if cs.is_empty() {
        return String::new();
    }
    format!(" Caveat: {}.", cs.join("; "))
}

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
    let caveats = caveat_txt(b);
    if b.get("significant").and_then(Value::as_bool) == Some(true) {
        let runner_up = s(b, "runner_up");
        let p_txt = p.map(|p| format!(", p={p:.4}")).unwrap_or_default();
        return Some(format!(
            "\n**Best: {label} ({mean:.2})** — significantly ahead of {runner_up}{p_txt}; \
             {correction}.{caveats}\n"
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
        "\nHighest mean: {label} ({mean:.2}) — {note}{p_txt}.{caveats}\n"
    ))
}

/// The frontier's per-row verdict, as two label lists: what is on the non-dominated set, and what
/// could not be placed on it at all. Labels only — the membership decision is made in the runner,
/// and this layer is not given the axes it was made on, so it cannot second-guess it.
fn frontier_marks(frontier: Option<&Value>) -> (Vec<&str>, Vec<&str>) {
    let arr = |key: &str| {
        frontier
            .and_then(|f| f.get(key))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    };
    let on = arr("non_dominated")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let off = arr("excluded")
        .iter()
        .filter_map(|e| opt_s(e, "label"))
        .collect();
    (on, off)
}

/// The rows the frontier refused to place, named with the reason. An exclusion the reader cannot
/// see is indistinguishable from a target that simply lost — and the excluded row is usually the
/// one that looked cheapest.
fn exclusion_note(frontier: &Value) -> Option<String> {
    let ex = frontier
        .get("excluded")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())?;
    let items: Vec<String> = ex
        .iter()
        .map(|e| format!("{} ({})", s(e, "label"), s(e, "reason")))
        .collect();
    Some(format!(
        "\n_Off the cost–quality frontier: {}._\n",
        items.join("; ")
    ))
}

/// The cheapest-sufficient sentence, printed from the object it was handed. Like [`winner_line`] it
/// derives nothing — but unlike a winner, this claim rests on an *absence* of evidence, so the case
/// count and the corrected α travel with it always, and a run that could separate nothing at all
/// loses the bold.
fn recommendation_line(rec: Option<&Value>) -> Option<String> {
    let r = rec.filter(|r| r.is_object())?;
    let n = u(r, "n_cases");
    let note = s(r, "note");
    let untested: Vec<&str> = r
        .get("undecidable")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| opt_s(x, "label")).collect())
        .unwrap_or_default();
    // Neither sufficient nor insufficient: said, never dropped from the list of things considered.
    let tail = if untested.is_empty() {
        String::new()
    } else {
        format!(
            "_Untested against the best target (not scored on the same cases): {}._\n",
            untested.join(", ")
        )
    };
    let Some(label) = opt_s(r, "label") else {
        return Some(format!(
            "\nNo cheapest-sufficient recommendation — {note}.\n{tail}"
        ));
    };
    let cost = opt_f(r, "cost_per_case_usd")
        .map(|c| format!(" ({}/case)", money(c)))
        .unwrap_or_default();
    let power = format!(
        "{}{}n={n} case(s)",
        opt_f(r, "p_value")
            .map(|p| format!("p={p:.4}, "))
            .unwrap_or_default(),
        opt_f(r, "alpha")
            .map(|a| format!("α={a:.4}, "))
            .unwrap_or_default(),
    );
    if opt_b(r, "all_candidates_indistinguishable") == Some(true) {
        return Some(format!(
            "\nCheapest sufficient: {label}{cost} — {note} ({power}).\n{tail}"
        ));
    }
    Some(format!(
        "\n**Cheapest sufficient: {label}{cost}** — {note} ({power}).\n{tail}"
    ))
}

pub(crate) fn leaderboard(v: &Value) -> Option<String> {
    let targets = v.get("targets")?.as_array()?;
    if targets.is_empty() {
        return Some("_No comparison targets._".to_string());
    }
    let n_cases = v.get("n_cases").and_then(Value::as_u64).unwrap_or(0);

    // The Effort column appears only when some target declared a level. A matrix with no effort axis
    // keeps the table it always had; one that has the axis must never hide it, because two rows of
    // one model at two efforts are otherwise indistinguishable in every column that follows.
    let has_effort = targets.iter().any(|r| r.get("effort").is_some());
    let mut cols = vec![("Target", Align::Left)];
    if has_effort {
        cols.push(("Effort", Align::Left));
    }
    cols.extend([
        ("Mean", Align::Right),
        ("Pass%", Align::Right),
        ("Agree", Align::Right),
        ("Gen$", Align::Right),
        ("Judge$", Align::Right),
        ("p50", Align::Right),
        ("Err", Align::Right),
    ]);
    // Appended last, and only when the runner handed us a frontier: the column order every existing
    // reader (and every stored table) knows stays exactly where it was.
    let frontier = v.get("frontier").filter(|f| f.is_object());
    let (on_frontier, excluded) = frontier_marks(frontier);
    if frontier.is_some() {
        cols.push(("Front", Align::Left));
    }
    let mut t = Table::new(&cols);
    // Best = highest mean among targets that didn't error out every case (mirrors the runner's rule).
    let mut best: Option<(&str, f64)> = None;
    for r in targets {
        let label = s(r, "label");
        let mean = f(r, "mean");
        let errored = u(r, "errored");
        if errored < n_cases && best.is_none_or(|(_, bm)| mean > bm) {
            best = Some((label, mean));
        }
        let mut cells = vec![label.to_string()];
        if has_effort {
            // A target with no level ran at the provider's default — an em dash, never a guessed
            // level and never a blank that reads as the same as its neighbour.
            cells.push(
                r.get("effort")
                    .and_then(Value::as_str)
                    .unwrap_or("—")
                    .to_string(),
            );
        }
        cells.extend([
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
        if frontier.is_some() {
            // `—` is an exclusion, not a loss: the row was never placed on the surface, and the
            // reason is spelled out under the table.
            cells.push(
                if excluded.contains(&label) {
                    "—"
                } else if on_frontier.contains(&label) {
                    "●"
                } else {
                    "·"
                }
                .to_string(),
            );
        }
        t.row(cells);
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
    // The second sentence: not "which target won" but "how cheap can I go", which is a different
    // question with a different answer whenever the cheap row sits inside the noise of the dear one.
    if let Some(line) = frontier.and_then(exclusion_note) {
        out.push_str(&line);
    }
    if let Some(line) = recommendation_line(v.get("recommendation")) {
        out.push_str(&line);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{leaderboard, recommendation_line, winner_line};
    use serde_json::json;

    /// **A tested `best` that rests on a subset must say so in the rendered line.**
    ///
    /// The significant branch used to return early with a bare "significantly ahead", so a claim
    /// whose targets errored on different cases — paired over their intersection, with cases
    /// dropped — rendered as an unqualified winner. The runner printed the caveat to its own stdout,
    /// which does not help the two other consumers of this table: the CLI and MCP render from here.
    #[test]
    fn a_significant_best_still_prints_the_caveat_it_carries() {
        let line = winner_line(
            Some(&json!({
                "label": "a", "mean": 0.85, "runner_up": "b", "significant": true,
                "p_value": 0.0012, "n_cases": 4, "cases_dropped": 2,
                "correction": "Bonferroni over 1 target pair(s)",
                "caveats": ["2 case(s) were judged by only one of the two targets"],
            })),
            None,
        )
        .expect("a claim renders");
        assert!(line.contains("**Best: a (0.85)**"), "{line}");
        assert!(
            line.contains("Caveat: 2 case(s) were judged by only one of the two targets."),
            "the subset reaches the RENDERED line, not just the runner's stdout: {line}"
        );
    }

    /// The same on the weaker branch, and both caveats when there are two.
    #[test]
    fn an_untested_claim_prints_its_caveats_too() {
        let line = winner_line(
            Some(&json!({
                "label": "a", "mean": 0.5, "significant": false,
                "note": "no significant difference from the runner-up",
                "caveats": ["first thing", "second thing"],
            })),
            None,
        )
        .expect("a claim renders");
        assert!(
            line.contains("Caveat: first thing; second thing."),
            "{line}"
        );
    }

    /// And a claim with no caveats gains no punctuation and no empty "Caveat:" — the compatibility
    /// half, which is what stops this being a cosmetic change to every table that already existed.
    #[test]
    fn a_claim_without_caveats_is_unchanged() {
        let clean = winner_line(
            Some(&json!({
                "label": "a", "mean": 0.85, "runner_up": "b", "significant": true,
                "p_value": 0.0012, "correction": "Bonferroni over 1 target pair(s)",
            })),
            None,
        )
        .expect("a claim renders");
        assert!(!clean.contains("Caveat"), "{clean}");
        assert!(
            clean.ends_with("Bonferroni over 1 target pair(s).\n"),
            "{clean}"
        );
        // An empty array is the same as an absent one.
        let empty = winner_line(
            Some(&json!({
                "label": "a", "mean": 0.85, "runner_up": "b", "significant": true,
                "p_value": 0.0012, "correction": "Bonferroni over 1 target pair(s)",
                "caveats": [],
            })),
            None,
        )
        .expect("a claim renders");
        assert_eq!(clean, empty, "an empty caveat list changes nothing");
    }

    /// **The compatibility gate.** A summary with no `frontier`/`recommendation` — every stored
    /// table the CLI and MCP render, and every run made before this existed — must produce the
    /// *bytes* it produced before, not merely something that looks similar. The literal below was
    /// captured from the previous build.
    #[test]
    fn a_matrix_with_no_frontier_renders_byte_identically() {
        let input = json!({
            "n_cases": 4,
            "targets": [
                { "label": "cheap", "mean": 0.80, "pass_rate": 0.75, "agreement": 0.9,
                  "gen_cost_usd": 0.004, "judge_cost_usd": 0.02, "p50_latency_ms": 300,
                  "errored": 0 },
                { "label": "dear", "mean": 0.84, "pass_rate": 0.75, "agreement": 0.9,
                  "gen_cost_usd": 0.4, "judge_cost_usd": 0.02, "p50_latency_ms": 900,
                  "errored": 0 }
            ],
            "status": "no_baseline",
        });
        const BEFORE: &str = concat!(
            "### Comparison — 4 case(s)\n\n",
            "| Target | Mean | Pass% | Agree |     Gen$ | Judge$ |   p50 | Err |\n",
            "|--------|-----:|------:|------:|---------:|-------:|------:|----:|\n",
            "| cheap  | 0.80 |   75% |  0.90 | $0.00400 |  $0.02 | 300ms |   0 |\n",
            "| dear   | 0.84 |   75% |  0.90 |    $0.40 |  $0.02 | 900ms |   0 |\n",
            "\n**Highest mean: dear (0.84)** — not tested for significance.\n",
        );
        assert_eq!(leaderboard(&input).unwrap(), BEFORE);
    }

    /// The marker column exists only to answer "which of these rows is even worth considering", so
    /// a dominated row and an *excluded* row must not read the same: one lost the trade-off, the
    /// other was never placed on it and the reason is printed underneath.
    #[test]
    fn frontier_rows_are_marked_and_exclusions_are_named() {
        let md = leaderboard(&json!({
            "n_cases": 6,
            "targets": [
                { "label": "cheap", "mean": 0.77, "errored": 0 },
                { "label": "mid", "mean": 0.70, "errored": 0 },
                { "label": "mystery", "mean": 0.95, "errored": 0 },
            ],
            "frontier": {
                "non_dominated": ["cheap"],
                "excluded": [{ "label": "mystery", "reason": "no price-book entry for its model" }],
            },
        }))
        .unwrap();
        assert!(md.contains("Front"), "the marker column appears: {md}");
        // The last cell of each row, unpadded.
        let mark = |l: &str| {
            md.lines()
                .find(|x| x.contains(l))
                .unwrap_or_default()
                .rsplit('|')
                .nth(1)
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        assert_eq!(mark("| cheap"), "●", "{md}");
        assert_eq!(mark("| mid"), "·", "dominated, not excluded: {md}");
        assert_eq!(mark("| mystery"), "—", "excluded, not merely beaten: {md}");
        assert!(
            md.contains("Off the cost–quality frontier: mystery (no price-book entry"),
            "the reason travels with the name: {md}"
        );
    }

    /// The recommendation is printed verbatim from the object, with the power that produced it. It
    /// is an absence of evidence, so the case count and corrected α are not optional decoration.
    #[test]
    fn the_recommendation_prints_its_power_and_derives_nothing() {
        let line = recommendation_line(Some(&json!({
            "label": "haiku", "best": "opus", "cost_per_case_usd": 0.00042,
            "p_value": 0.3100, "alpha": 0.016667, "n_cases": 20,
            "all_candidates_indistinguishable": false,
            "note": "the run could not show opus ahead of it at the corrected α",
            "undecidable": [{ "label": "flaky", "reason": "not scored on the same cases" }],
        })))
        .unwrap();
        assert!(line.contains("**Cheapest sufficient: haiku ($0.00042/case)**"));
        assert!(line.contains("could not show opus ahead of it"));
        assert!(
            line.contains("p=0.3100") && line.contains("α=0.0167") && line.contains("n=20"),
            "power is disclosed on the claim: {line}"
        );
        assert!(
            line.contains("Untested against the best target") && line.contains("flaky"),
            "an unpairable candidate is reported, not skipped: {line}"
        );
    }

    /// A run that could separate *nothing* has measured its own sample size, not the models — so
    /// the sentence loses its bold, and says which of the two it is.
    #[test]
    fn a_run_that_separates_nothing_loses_the_bold() {
        let line = recommendation_line(Some(&json!({
            "label": "cheapest", "best": "dearest", "cost_per_case_usd": 0.0001,
            "p_value": 0.9, "alpha": 0.05, "n_cases": 3,
            "all_candidates_indistinguishable": true,
            "note": "every candidate on the frontier was indistinguishable from dearest at n=3 — \
                     that is a fact about this run's power, not a finding about the models",
        })))
        .unwrap();
        assert!(
            !line.contains("**"),
            "no bold on an undiscriminating run: {line}"
        );
        assert!(line.contains("fact about this run's power"));
    }

    /// No recommendation prints the reason, never silence — and never a bold row.
    #[test]
    fn a_refused_recommendation_says_why() {
        let line = recommendation_line(Some(&json!({
            "label": serde_json::Value::Null, "n_cases": 4, "alpha": 0.05,
            "note": "the run was halted at its spend ceiling and scored only part of its cases",
            "undecidable": [],
        })))
        .unwrap();
        assert!(line.contains("No cheapest-sufficient recommendation"));
        assert!(line.contains("halted at its spend ceiling"));
        assert!(!line.contains("**"));
        // Nothing handed in at all → no line, exactly as `winner_line` behaves.
        assert!(recommendation_line(None).is_none());
    }

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

    /// **The reason the effort axis exists, at the point an operator reads it.** One model at two
    /// levels must be two visibly different rows — a table that printed `gpt-5` twice would make
    /// "is xhigh worth 4× low" unanswerable from the artefact that is supposed to answer it.
    #[test]
    fn two_efforts_of_one_model_are_readable_as_two_rows() {
        let md = leaderboard(&json!({
            "n_cases": 10,
            "targets": [
                { "label": "openai/gpt-5@low", "effort": "low", "mean": 0.71, "errored": 0 },
                { "label": "openai/gpt-5@high", "effort": "high", "mean": 0.88, "errored": 0 },
                { "label": "openai/gpt-4o", "mean": 0.65, "errored": 0 },
            ],
        }))
        .unwrap();
        assert!(md.contains("Effort"), "the column is present: {md}");
        assert!(md.contains("low") && md.contains("high"));
        // A target that declared no level ran at the provider's default — said, not left blank.
        assert!(md.contains('—'), "the default is stated, not implied: {md}");
    }

    /// A run with no effort axis keeps exactly the table it always had — no empty column.
    #[test]
    fn a_matrix_without_efforts_keeps_its_original_columns() {
        let md = leaderboard(&json!({
            "n_cases": 10, "targets": [{ "label": "a", "mean": 0.9, "errored": 0 }],
        }))
        .unwrap();
        assert!(!md.contains("Effort"), "no column nobody can fill: {md}");
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
