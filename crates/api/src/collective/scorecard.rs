//! Reducing one benchmark run scorecard to a publishable [`RunStat`].
//!
//! Everything here is a pure function over a `(Benchmark, BenchmarkRun)` pair: resolving the model
//! identity, deriving the coarse judge family, fingerprinting the rubric one-way, and reading the
//! rigor stamps out of the run report. The privacy shape lives in these derivations — a judge family
//! rather than a judge model, a hash rather than a rubric — so they are kept together and tested
//! together, apart from the endpoint that calls them ([`super::digest`]).

use serde_json::Value;
use sha2::{Digest, Sha256};

use lighttrack_core::{task_type_from, Benchmark, BenchmarkRun, RunStat};

/// Reduce one `(Benchmark, run)` to a [`RunStat`], or `None` when it can't contribute (no known
/// provider/model, no quality, or no cases).
pub(super) fn run_stat(bench: &Benchmark, run: &BenchmarkRun) -> Option<RunStat> {
    let (provider, model) = provider_model(bench, run)?;
    let quality = run.mean_score?;
    if run.n_cases == 0 {
        return None;
    }
    let cost_per_case_usd = run.cost_usd / run.n_cases as f64;
    Some(RunStat {
        provider,
        model,
        task_type: task_type_from(&bench.name, None),
        quality,
        pass_rate: run.pass_rate.unwrap_or(0.0),
        cost_per_case_usd,
        n_cases: run.n_cases,
        p50_latency_ms: run.p50_latency_ms,
        p95_latency_ms: run.p95_latency_ms,
        judge_provider: judge_provider_of(&bench.judge_model),
        rubric_fingerprint: rubric_fingerprint_of(bench),
        determinism: run
            .report
            .get("determinism")
            .and_then(Value::as_str)
            .and_then(lighttrack_core::canon_determinism),
        dataset_frozen: run.report.get("dataset_frozen").and_then(Value::as_bool),
        dataset_version: run
            .report
            .get("dataset_version")
            .and_then(Value::as_u64)
            .map(|v| v.min(u32::MAX as u64) as u32),
        significance_tested: significance_tested_of(&run.report),
    })
}

/// Whether a run's verdict was **significance-tested**: the report carries a two-sided interval
/// (`ci95`) over at least two scored cases. `n < 2` has no spread, so its "interval" is a point
/// dressed up as one — that counts as untested, not as tested. `None` when the run predates the
/// significance annotation entirely (no `n` recorded), so an old run reads as *unknown* rather than
/// being libelled as sloppy.
fn significance_tested_of(report: &Value) -> Option<bool> {
    let n = report.get("n").and_then(Value::as_u64)?;
    let has_ci = report
        .get("ci95")
        .and_then(Value::as_array)
        .is_some_and(|a| a.len() == 2);
    Some(n >= 2 && has_ci)
}

/// Classify a benchmark's `judge_model` (`[provider/]model`) into a coarse judge family — provider
/// only (`anthropic|openai|google|unknown`), never the full model, to limit fingerprinting. An
/// explicit `provider/` prefix wins; otherwise the family is inferred from the model name.
fn judge_provider_of(judge_model: &str) -> Option<String> {
    let m = judge_model.trim().to_lowercase();
    if m.is_empty() {
        return None;
    }
    let (prefix, name) = m.split_once('/').unwrap_or(("", m.as_str()));
    let canon_prefix = match prefix {
        "anthropic" | "claude" => Some("anthropic"),
        "openai" | "azure-openai" | "azure" => Some("openai"),
        "google" | "gemini" | "vertex" | "google-vertex" => Some("google"),
        _ => None,
    };
    if let Some(c) = canon_prefix {
        return Some(c.to_string());
    }
    let name = if name.is_empty() { m.as_str() } else { name };
    let family = if ["claude", "haiku", "sonnet", "opus"]
        .iter()
        .any(|k| name.contains(k))
    {
        "anthropic"
    } else if name.contains("gpt") || name.starts_with("o1") || name.starts_with("o3") {
        "openai"
    } else if name.contains("gemini") || name.contains("gemma") || name.contains("bison") {
        "google"
    } else {
        "unknown"
    };
    Some(family.to_string())
}

/// A short, one-way fingerprint of a benchmark's rubric shape — 8 hex of SHA-256 over the
/// whitespace-normalized rubric definition (or its id, if the text is empty). Lets two instances tell
/// whether they scored under the same rubric without either revealing the rubric text. `None` when the
/// benchmark carries no rubric at all.
fn rubric_fingerprint_of(bench: &Benchmark) -> Option<String> {
    let basis = if !bench.rubric.trim().is_empty() {
        bench
            .rubric
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        bench
            .rubric_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())?
            .to_string()
    };
    let mut h = Sha256::new();
    h.update(basis.as_bytes());
    Some(
        h.finalize()
            .iter()
            .take(4)
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// Resolve the model identity from the compare-mode run report, else the benchmark's single target.
fn provider_model(bench: &Benchmark, run: &BenchmarkRun) -> Option<(String, String)> {
    let from = |v: &Value| {
        let p = v
            .get("provider")
            .and_then(Value::as_str)?
            .trim()
            .to_string();
        let m = v.get("model").and_then(Value::as_str)?.trim().to_string();
        (!p.is_empty() && !m.is_empty()).then_some((p, m))
    };
    from(&run.report).or_else(|| from(&bench.target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn bench(name: &str, target: Value) -> Benchmark {
        Benchmark {
            id: "b1".into(),
            project_id: "p1".into(),
            name: name.into(),
            rubric: String::new(),
            judge_model: "haiku".into(),
            target,
            dataset_ref: None,
            rubric_id: None,
            dataset: vec![],
            baseline_score: None,
            created_at: Utc::now(),
        }
    }

    fn run(report: Value, mean: Option<f64>, cases: u32, cost: f64) -> BenchmarkRun {
        BenchmarkRun {
            id: "r1".into(),
            benchmark_id: "b1".into(),
            started_at: Utc::now(),
            finished_at: None,
            n_cases: cases,
            mean_score: mean,
            pass_rate: Some(0.8),
            cost_usd: cost,
            status: "compared".into(),
            p50_latency_ms: Some(700),
            p95_latency_ms: Some(1400),
            total_tokens: Some(1000),
            report,
        }
    }

    #[test]
    fn run_stat_reads_compare_report() {
        let b = bench("Nightly QA bench", Value::Null);
        let r = run(
            json!({"provider":"anthropic","model":"haiku"}),
            Some(0.82),
            20,
            0.4,
        );
        let s = run_stat(&b, &r).unwrap();
        assert_eq!(
            (s.provider.as_str(), s.model.as_str()),
            ("anthropic", "haiku")
        );
        assert_eq!(s.task_type, "qa");
        assert!((s.cost_per_case_usd - 0.02).abs() < 1e-9); // 0.4 / 20
    }

    #[test]
    fn run_stat_falls_back_to_target_then_skips() {
        // No report identity, but the benchmark's single target carries it.
        let b = bench("Summaries", json!({"provider":"openai","model":"gpt-x"}));
        let r = run(Value::Null, Some(0.7), 10, 0.1);
        let s = run_stat(&b, &r).unwrap();
        assert_eq!(s.model, "gpt-x");
        assert_eq!(s.task_type, "summarization");
        // No identity anywhere → skipped.
        let b2 = bench("x", Value::Null);
        assert!(run_stat(&b2, &run(Value::Null, Some(0.7), 10, 0.1)).is_none());
        // No quality → skipped.
        assert!(run_stat(&b, &run(json!({"provider":"a","model":"m"}), None, 10, 0.1)).is_none());
    }

    #[test]
    fn run_stat_reads_rigor_out_of_the_run_report() {
        let b = bench("QA bench", Value::Null);
        let report = json!({
            "provider": "anthropic", "model": "haiku",
            "determinism": "exact", "dataset_frozen": true, "dataset_version": 3,
            "n": 20, "ci95": [0.78, 0.86],
        });
        let s = run_stat(&b, &run(report, Some(0.82), 20, 0.4)).unwrap();
        assert_eq!(s.determinism.as_deref(), Some("exact"));
        assert_eq!(s.dataset_frozen, Some(true));
        assert_eq!(s.dataset_version, Some(3));
        assert_eq!(s.significance_tested, Some(true));
        // A run that predates the significance annotation is *unknown*, never libelled as untested.
        let bare = json!({ "provider": "anthropic", "model": "haiku" });
        let s = run_stat(&b, &run(bare, Some(0.82), 20, 0.4)).unwrap();
        assert!(s.determinism.is_none());
        assert!(s.dataset_frozen.is_none());
        assert!(s.significance_tested.is_none());
    }

    #[test]
    fn significance_needs_an_interval_over_more_than_one_case() {
        // n=1 has no spread: its "interval" is a point dressed up as one.
        assert_eq!(
            significance_tested_of(&json!({ "n": 1, "ci95": [0.8, 0.8] })),
            Some(false)
        );
        assert_eq!(
            significance_tested_of(&json!({ "n": 20 })),
            Some(false),
            "no interval recorded"
        );
        assert_eq!(
            significance_tested_of(&json!({ "n": 20, "ci95": [0.7, 0.9] })),
            Some(true)
        );
        assert_eq!(
            significance_tested_of(&json!({})),
            None,
            "unrecorded ≠ untested"
        );
    }

    #[test]
    fn judge_provider_classification() {
        assert_eq!(
            judge_provider_of("anthropic/claude-haiku-4-5").as_deref(),
            Some("anthropic")
        );
        assert_eq!(judge_provider_of("haiku").as_deref(), Some("anthropic"));
        assert_eq!(judge_provider_of("gpt-4o").as_deref(), Some("openai"));
        assert_eq!(
            judge_provider_of("openai/o3-mini").as_deref(),
            Some("openai")
        );
        assert_eq!(
            judge_provider_of("gemini-1.5-pro").as_deref(),
            Some("google")
        );
        assert_eq!(
            judge_provider_of("some-local-llm").as_deref(),
            Some("unknown")
        );
        assert_eq!(judge_provider_of("  "), None);
    }
}
