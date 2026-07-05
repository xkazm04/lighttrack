//! `calibrate`: measure judge↔human agreement on a labeled set so a rubric can be *trusted* before
//! it's used for gating. Judge-only — the outputs are given and the judge re-scores them, then we
//! compare to the human labels (Cohen's κ + correlation). See docs/BENCHMARK_FRAMEWORK.md §3.

use std::fs;

use anyhow::{bail, Context, Result};

use lighttrack_core::{agreement, CalibrationItem, ModelPriceRow, Rubric};
use lighttrack_engine::{build_judge_prompt, parse_judge_spec, run_judge, run_rubric_judge, EngineConfig};

use crate::cli::Cli;
use crate::http::get;
use crate::util::{price_gen_cost, short};

#[allow(clippy::too_many_arguments)]
pub(crate) fn calibrate(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    file: &str,
    rubric_text: Option<&str>,
    rubric_id: Option<&str>,
    threshold: f64,
    kappa_bar: f64,
    samples: u32,
    report_path: Option<&str>,
) -> Result<()> {
    let items = load_items(file)?;
    if items.is_empty() {
        bail!("no calibration items in {file}");
    }

    // Structured rubric (fetched from the API) takes precedence over freeform text.
    let rubric: Option<Rubric> = match rubric_id {
        Some(id) => Some(get(cli, http, &format!("/v1/rubrics/{id}"))?),
        None => None,
    };
    if rubric.is_none() && rubric_text.is_none() {
        bail!("provide --rubric \"<criteria>\" or --rubric-id <id>");
    }

    let (jp, jm) = parse_judge_spec(&engine.model);
    let prices: Vec<ModelPriceRow> = get(cli, http, "/v1/prices").unwrap_or_default();

    println!(
        "calibrating {} item(s) — judge={jp}/{jm}, threshold={threshold:.2}, \u{3ba}-bar={kappa_bar:.2}{}",
        items.len(),
        rubric.as_ref().map(|r| format!(", rubric={}", r.name)).unwrap_or_default(),
    );
    println!("  {:<10}  {:>6}  {:>6}  {:>7}  {:<5}", "item", "human", "judge", "delta", "agree");

    let mut pairs: Vec<(f64, f64)> = Vec::new();
    let mut cost = 0.0_f64;
    let mut skipped = 0u32;
    for (i, it) in items.iter().enumerate() {
        let (judge_score, jc) =
            match judge_item(engine, &jp, &jm, &rubric, rubric_text, it, samples, &prices) {
                Ok(pair) => pair,
                // A phantom 0.0 here would poison kappa/MAE; drop the item from the calibration set.
                Err(e) => {
                    eprintln!("  item #{} skipped — judge output unparseable: {e}", i + 1);
                    skipped += 1;
                    continue;
                }
            };
        cost += jc;
        let agree = (it.human_score >= threshold) == (judge_score >= threshold);
        pairs.push((it.human_score, judge_score));
        let label = it
            .note
            .as_deref()
            .map(|s| short(s).to_string())
            .unwrap_or_else(|| format!("#{}", i + 1));
        println!(
            "  {:<10}  {:>6.2}  {:>6.2}  {:>+7.2}  {:<5}",
            label,
            it.human_score,
            judge_score,
            judge_score - it.human_score,
            if agree { "ok" } else { "MISS" },
        );
    }

    if skipped > 0 {
        println!("  note: {skipped} item(s) skipped — judge output was unparseable.");
    }
    let a = agreement(&pairs, threshold, kappa_bar);
    println!(
        "\nagreement (n={}):  \u{3ba}={:.3}  pearson={:.3}  MAE={:.3}  RMSE={:.3}  bias={:+.3}",
        a.n, a.cohen_kappa, a.pearson, a.mae, a.rmse, a.bias,
    );
    println!(
        "  agreement_rate={:.0}%  human_pass={:.0}%  judge_pass={:.0}%  judge_cost=${cost:.5}",
        a.agreement_rate * 100.0,
        a.human_pass_rate * 100.0,
        a.judge_pass_rate * 100.0,
    );
    println!(
        "  verdict: {} (\u{3ba} {:.3} {} bar {:.2})",
        if a.trusted { "TRUSTED" } else { "NOT TRUSTED" },
        a.cohen_kappa,
        if a.trusted { ">=" } else { "<" },
        kappa_bar,
    );
    if a.bias.abs() > 0.1 {
        println!(
            "  note: judge is {} than humans by {:.2} on average \u{2014} consider tightening the rubric.",
            if a.bias > 0.0 { "more generous" } else { "harsher" },
            a.bias.abs(),
        );
    }

    if let Some(p) = report_path {
        let report = serde_json::json!({
            "file": file,
            "judge": format!("{jp}/{jm}"),
            "rubric": rubric.as_ref().map(|r| r.name.clone()),
            "samples": samples,
            "agreement": a,
            "judge_cost_usd": cost,
        });
        fs::write(p, serde_json::to_string_pretty(&report)?).with_context(|| format!("writing {p}"))?;
        println!("wrote report to {p}");
    }
    Ok(())
}

/// Judge one item via the structured rubric (if any) or freeform criteria text; returns
/// (normalized 0..1 score, judge cost). Cost is priced from the book when the provider gives no $.
#[allow(clippy::too_many_arguments)]
fn judge_item(
    engine: &EngineConfig,
    jp: &str,
    jm: &str,
    rubric: &Option<Rubric>,
    rubric_text: Option<&str>,
    it: &CalibrationItem,
    samples: u32,
    prices: &[ModelPriceRow],
) -> Result<(f64, f64)> {
    if let Some(r) = rubric {
        let o = run_rubric_judge(
            engine, jp, jm, r, &it.input, it.expected.as_deref(), &it.output, samples,
        )
        .context("rubric judge failed")?;
        let jc = o
            .cost_usd
            .unwrap_or_else(|| price_gen_cost(prices, jp, jm, o.input_tokens, o.output_tokens));
        Ok((o.overall, jc))
    } else {
        let prompt = build_judge_prompt(rubric_text.unwrap_or(""), &it.input, &it.output);
        let v = run_judge(engine, jp, jm, &prompt).context("judge failed")?;
        let norm = if v.verdict.max > 0.0 {
            v.verdict.score / v.verdict.max
        } else {
            v.verdict.score
        };
        let jc = v
            .cost_usd
            .unwrap_or_else(|| price_gen_cost(prices, jp, jm, v.input_tokens, v.output_tokens));
        Ok((norm, jc))
    }
}

/// Load calibration items from a JSONL file (one object per line) or a JSON array file.
fn load_items(file: &str) -> Result<Vec<CalibrationItem>> {
    let text = fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    parse_items(&text, file)
}

/// Parse calibration items from file contents: a JSON array (when the text starts with `[`) or JSONL
/// (one object per line; blank lines and `//`-comment lines are skipped). `file` is used only for
/// error context. I/O-free so it can be unit-tested without a temp file or a live provider.
fn parse_items(text: &str, file: &str) -> Result<Vec<CalibrationItem>> {
    if text.trim_start().starts_with('[') {
        return serde_json::from_str(text).with_context(|| format!("{file}: invalid JSON array of items"));
    }
    let mut items = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let l = line.trim();
        if l.is_empty() || l.starts_with("//") {
            continue;
        }
        items.push(
            serde_json::from_str(l).with_context(|| format!("{file}:{} \u{2014} invalid item", n + 1))?,
        );
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::parse_items;

    #[test]
    fn parses_jsonl_skipping_blanks_and_comments() {
        let text = "\
// a leading comment
{\"input\":\"a\",\"output\":\"x\",\"human_score\":0.9}

{\"input\":\"b\",\"output\":\"y\",\"human_score\":0.2,\"note\":\"case b\"}
";
        let items = parse_items(text, "f.jsonl").unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].input, "a");
        assert!((items[0].human_score - 0.9).abs() < 1e-9);
        assert_eq!(items[1].note.as_deref(), Some("case b"));
    }

    #[test]
    fn parses_json_array_form() {
        let text = r#"[
            {"input":"a","output":"x","human_score":0.5},
            {"input":"b","output":"y","human_score":0.8}
        ]"#;
        let items = parse_items(text, "f.json").unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].output, "y");
    }

    #[test]
    fn array_detection_ignores_leading_whitespace() {
        let items = parse_items("   \n  [{\"input\":\"a\",\"output\":\"x\",\"human_score\":1.0}]", "f").unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn empty_input_yields_no_items() {
        assert!(parse_items("", "f").unwrap().is_empty());
        assert!(parse_items("// only a comment\n\n", "f").unwrap().is_empty());
    }

    #[test]
    fn malformed_line_errors_with_line_number() {
        let text = "{\"input\":\"a\",\"output\":\"x\",\"human_score\":0.9}\nnot json";
        let err = parse_items(text, "bad.jsonl").unwrap_err();
        assert!(err.to_string().contains("bad.jsonl:2"), "got: {err}");
    }

    #[test]
    fn missing_required_field_errors() {
        // `human_score` is required by CalibrationItem.
        assert!(parse_items("{\"input\":\"a\",\"output\":\"x\"}", "f").is_err());
    }
}
