//! `lt-runner` — the LightTrack scoring runner.
//!
//! Fetches events from the API, judges them with `claude -p` (the unbudgeted LLM engine), and
//! POSTs the resulting scores back. This is the component that runs locally / on the e2-micro
//! (where `claude` is authenticated), keeping the API (Cloud Run) free of any model invocation.
//!
//! Examples:
//!   lt-runner score --project <id> --rubric "Is the answer correct and helpful?" --limit 10
//!   lt-runner score-text --rubric "Concise and correct?" --input "2+2?" --output "4" --project <id>
//!
//! Config (flags or env): --base LIGHTTRACK_URL, --key LIGHTTRACK_KEY,
//!   --model LIGHTTRACK_JUDGE_MODEL (default haiku), --claude-bin (default claude).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use lighttrack_anon::scrub;
use lighttrack_core::{Benchmark, BenchmarkCase, DatasetItem, LlmEvent};
use lighttrack_engine::{build_eval_prompt, build_judge_prompt, run_judge, run_text, EngineConfig};

#[derive(Parser)]
#[command(name = "lt-runner", about = "LightTrack scoring runner (claude -p judge)")]
struct Cli {
    #[arg(long, env = "LIGHTTRACK_URL", default_value = "http://127.0.0.1:8787")]
    base: String,
    #[arg(long, env = "LIGHTTRACK_KEY")]
    key: Option<String>,
    #[arg(long, env = "LIGHTTRACK_JUDGE_MODEL", default_value = "haiku")]
    model: String,
    /// Path to the claude executable. On Windows the default auto-resolves the npm `claude.exe`
    /// (the `claude.cmd`/`.ps1` shims can't be invoked directly from a child process).
    #[arg(long, env = "LIGHTTRACK_CLAUDE_BIN", default_value = "claude")]
    claude_bin: String,
    /// Pass --bare to claude (cheap: skips ~40k token context load, but needs ANTHROPIC_API_KEY).
    #[arg(long)]
    bare: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Score recent events (those with both input and output) for a project.
    Score {
        #[arg(long)]
        rubric: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Score an ad-hoc input/output pair (not tied to a stored event).
    ScoreText {
        #[arg(long)]
        rubric: String,
        #[arg(long)]
        input: String,
        #[arg(long)]
        output: String,
        #[arg(long)]
        project: String,
    },
    /// Run a stored benchmark: judge each case, aggregate a scorecard, record a run.
    Bench {
        #[arg(long)]
        benchmark: String,
    },
    /// Build a dataset by sampling real events and anonymizing them.
    Dataset {
        #[command(subcommand)]
        action: DatasetCmd,
    },
}

#[derive(Subcommand)]
enum DatasetCmd {
    /// Sample N recent events for a project, scrub PII, and freeze a new dataset.
    Build {
        #[arg(long)]
        project: String,
        #[arg(long)]
        name: String,
        #[arg(long, default_value_t = 50)]
        n: usize,
        /// Add an LLM (claude -p) anonymization pass for names/free-text PII the regex misses.
        #[arg(long)]
        llm_scrub: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let engine = EngineConfig {
        claude_bin: resolve_claude_bin(&cli.claude_bin),
        model: cli.model.clone(),
        bare: cli.bare,
    };
    let http = reqwest::blocking::Client::new();

    match &cli.cmd {
        Cmd::Score {
            rubric,
            project,
            limit,
        } => score_recent(&cli, &http, &engine, rubric, project.as_deref(), *limit),
        Cmd::ScoreText {
            rubric,
            input,
            output,
            project,
        } => {
            let outcome = judge_one(&engine, rubric, input, output)?;
            let score = build_score(project, None, rubric, &outcome);
            let stored = post(&cli, &http, "/v1/scores", &score)?;
            println!("posted score: {}", serde_json::to_string_pretty(&stored)?);
            Ok(())
        }
        Cmd::Bench { benchmark } => run_benchmark(&cli, &http, &engine, benchmark),
        Cmd::Dataset { action } => match action {
            DatasetCmd::Build {
                project,
                name,
                n,
                llm_scrub,
            } => build_dataset(&cli, &http, &engine, project, name, *n, *llm_scrub),
        },
    }
}

/// Sample events, scrub PII (regex always; optional LLM pass), and freeze a dataset.
fn build_dataset(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    project: &str,
    name: &str,
    n: usize,
    llm_scrub: bool,
) -> Result<()> {
    let events: Vec<LlmEvent> = get(cli, http, &format!("/v1/events?project={project}&limit={n}"))?;
    let with_input: Vec<&LlmEvent> = events.iter().filter(|e| e.input.is_some()).collect();
    println!(
        "sampling {} of {} event(s) with input from '{project}' (llm_scrub={llm_scrub})",
        with_input.len(),
        events.len()
    );

    let created: Value = post(
        cli,
        http,
        &format!("/v1/projects/{project}/datasets"),
        &json!({ "name": name, "source": "events:recent" }),
    )?;
    let dsid = created
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("dataset create returned no id"))?
        .to_string();

    let (mut built, mut total_redactions) = (0u32, 0usize);
    let method = if llm_scrub { "regex+llm" } else { "regex" };
    for ev in with_input {
        let (input_clean, r_in) = scrub_text(&value_to_text(ev.input.as_ref().unwrap()), llm_scrub, engine)?;
        let (output_clean, r_out) = match ev.output.as_ref() {
            Some(o) => {
                let (c, r) = scrub_text(&value_to_text(o), llm_scrub, engine)?;
                (Some(c), r)
            }
            None => (None, 0),
        };
        let redactions = r_in + r_out;
        total_redactions += redactions;
        let item = json!({
            "input": input_clean,
            "output": output_clean,
            "source_event_id": ev.id,
            "tags": ev.tags,
            "anonymization": { "method": method, "redactions": redactions },
        });
        post(cli, http, &format!("/v1/datasets/{dsid}/items"), &item)?;
        built += 1;
        println!("  + item from {} ({redactions} redactions)", short(&ev.id));
    }

    post(cli, http, &format!("/v1/datasets/{dsid}/freeze"), &json!({}))?;
    println!("\nbuilt dataset {dsid}: {built} items, {total_redactions} total redactions, frozen");
    Ok(())
}

/// Regex scrub (always) + optional LLM scrub pass. Returns (clean_text, redaction_count).
fn scrub_text(text: &str, llm: bool, engine: &EngineConfig) -> Result<(String, usize)> {
    let res = scrub(text);
    let mut out = res.text;
    let mut redactions = res.redactions;
    if llm {
        let prompt = format!(
            "Rewrite the text below, replacing any remaining personally identifiable information \
(names of people, organizations, precise locations, account/order numbers) with generic \
placeholders like <NAME>, <ORG>, <LOCATION>, <ID>. Preserve meaning and structure. \
Return ONLY the rewritten text, with no preamble.\n\nTEXT:\n{out}"
        );
        let outcome = run_text(engine, &prompt).context("LLM anonymization (claude -p) failed")?;
        let trimmed = outcome.text.trim();
        if !trimmed.is_empty() {
            // Count placeholders the LLM added beyond what regex produced (rough signal).
            let added = trimmed.matches('<').count().saturating_sub(out.matches('<').count());
            out = trimmed.to_string();
            redactions += added;
        }
    }
    Ok((out, redactions))
}

fn run_benchmark(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    benchmark_id: &str,
) -> Result<()> {
    let bench: Benchmark = get(cli, http, &format!("/v1/benchmarks/{benchmark_id}"))?;

    // Cases come from the inline dataset, or are resolved from a referenced stored dataset.
    let cases: Vec<BenchmarkCase> = if !bench.dataset.is_empty() {
        bench.dataset.clone()
    } else if let Some(ds) = bench.dataset_ref.as_deref() {
        let items: Vec<DatasetItem> = get(cli, http, &format!("/v1/datasets/{ds}/items"))?;
        items
            .into_iter()
            .map(|it| BenchmarkCase {
                input: it.input,
                expected: it.expected,
                output: it.output,
            })
            .collect()
    } else {
        Vec::new()
    };

    println!(
        "benchmark '{}' — {} case(s), judge={}, baseline={}",
        bench.name,
        cases.len(),
        bench.judge_model,
        bench
            .baseline_score
            .map(|b| format!("{b:.3}"))
            .unwrap_or_else(|| "none".into())
    );

    // Judge with the benchmark's own model.
    let bench_engine = EngineConfig {
        claude_bin: engine.claude_bin.clone(),
        model: bench.judge_model.clone(),
        bare: engine.bare,
    };

    let (mut sum, mut n, mut passes, mut cost) = (0.0_f64, 0u32, 0u32, 0.0_f64);
    let mut latencies: Vec<u64> = Vec::new();
    let mut total_tokens: u64 = 0;
    for (i, case) in cases.iter().enumerate() {
        let output = match &case.output {
            Some(o) => o,
            None => {
                println!("  case {}: skipped (no output)", i + 1);
                continue;
            }
        };
        let prompt = build_eval_prompt(&bench.rubric, &case.input, case.expected.as_deref(), output);
        let outcome = run_judge(&bench_engine, &prompt).context("judge (claude -p) failed")?;
        let norm = if outcome.verdict.max > 0.0 {
            outcome.verdict.score / outcome.verdict.max
        } else {
            outcome.verdict.score
        };
        sum += norm;
        n += 1;
        if outcome.verdict.pass {
            passes += 1;
        }
        cost += outcome.cost_usd.unwrap_or(0.0);
        if let Some(l) = outcome.latency_ms {
            latencies.push(l);
        }
        total_tokens += outcome.input_tokens.unwrap_or(0) + outcome.output_tokens.unwrap_or(0);
        println!(
            "  case {}: score={:.2} pass={} {}ms :: {}",
            i + 1,
            norm,
            outcome.verdict.pass,
            outcome.latency_ms.unwrap_or(0),
            outcome.verdict.reasoning
        );
        // Persist each case score under a bench-scoped rubric.
        let score = json!({
            "project_id": bench.project_id,
            "rubric": format!("bench:{}", bench.name),
            "value": outcome.verdict.score,
            "max": outcome.verdict.max,
            "pass": outcome.verdict.pass,
            "reasoning": outcome.verdict.reasoning,
            "scored_by": outcome.model,
            "cost_usd": outcome.cost_usd,
        });
        post(cli, http, "/v1/scores", &score)?;
    }

    let mean = if n > 0 { sum / n as f64 } else { 0.0 };
    let pass_rate = if n > 0 { passes as f64 / n as f64 } else { 0.0 };
    let (p50, p95) = percentiles(&mut latencies);
    let status = match bench.baseline_score {
        Some(b) if mean + 1e-9 < b => "regressed",
        Some(_) => "passed",
        None => "no_baseline",
    };

    println!(
        "\nscorecard: mean={mean:.3}  pass_rate={:.0}%  cost=${cost:.5}  p50={}ms p95={}ms  tokens={total_tokens}  status={status}",
        pass_rate * 100.0,
        p50.unwrap_or(0),
        p95.unwrap_or(0),
    );
    if let Some(b) = bench.baseline_score {
        println!(
            "baseline={b:.3} -> {}",
            if status == "regressed" {
                "REGRESSION"
            } else {
                "ok"
            }
        );
    }

    let run = json!({
        "benchmark_id": bench.id,
        "n_cases": n,
        "mean_score": mean,
        "pass_rate": pass_rate,
        "cost_usd": cost,
        "status": status,
        "p50_latency_ms": p50,
        "p95_latency_ms": p95,
        "total_tokens": total_tokens,
    });
    let stored = post(cli, http, "/v1/benchmark-runs", &run)?;
    println!(
        "recorded run {}",
        stored.get("id").and_then(|v| v.as_str()).unwrap_or("?")
    );
    Ok(())
}

fn score_recent(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    rubric: &str,
    project: Option<&str>,
    limit: usize,
) -> Result<()> {
    let mut path = format!("/v1/events?limit={limit}");
    if let Some(p) = project {
        path.push_str(&format!("&project={p}"));
    }
    let events: Vec<LlmEvent> = get(cli, http, &path)?;
    println!("fetched {} event(s) for scoring", events.len());

    let mut scored = 0;
    for ev in &events {
        let (input, output) = match (ev.input.as_ref(), ev.output.as_ref()) {
            (Some(i), Some(o)) => (value_to_text(i), value_to_text(o)),
            _ => {
                println!("  - {} skipped (no input/output content)", short(&ev.id));
                continue;
            }
        };
        print!("  - judging {} ({})... ", short(&ev.id), ev.model);
        let outcome = judge_one(engine, rubric, &input, &output)?;
        let score = build_score(&ev.project_id, Some(&ev.id), rubric, &outcome);
        post(cli, http, "/v1/scores", &score)?;
        scored += 1;
        println!(
            "score={:.2}/{:.0} pass={} cost={} :: {}",
            outcome.verdict.score,
            outcome.verdict.max,
            outcome.verdict.pass,
            outcome
                .cost_usd
                .map(|c| format!("${c:.5}"))
                .unwrap_or_else(|| "n/a".into()),
            outcome.verdict.reasoning
        );
    }
    println!("done: {scored} scored, {} skipped", events.len() - scored);
    Ok(())
}

fn judge_one(
    engine: &EngineConfig,
    rubric: &str,
    input: &str,
    output: &str,
) -> Result<lighttrack_engine::JudgeOutcome> {
    let prompt = build_judge_prompt(rubric, input, output);
    run_judge(engine, &prompt).context("judge (claude -p) failed")
}

fn build_score(
    project_id: &str,
    event_id: Option<&str>,
    rubric: &str,
    outcome: &lighttrack_engine::JudgeOutcome,
) -> Value {
    json!({
        "project_id": project_id,
        "event_id": event_id,
        "rubric": rubric,
        "value": outcome.verdict.score,
        "max": outcome.verdict.max,
        "pass": outcome.verdict.pass,
        "reasoning": outcome.verdict.reasoning,
        "scored_by": outcome.model,
        "cost_usd": outcome.cost_usd,
    })
}

/// p50/p95 of a latency sample (nearest-rank). Returns (None, None) if empty.
fn percentiles(latencies: &mut [u64]) -> (Option<u64>, Option<u64>) {
    if latencies.is_empty() {
        return (None, None);
    }
    latencies.sort_unstable();
    let pick = |p: f64| {
        let idx = (((latencies.len() - 1) as f64) * p).round() as usize;
        latencies[idx.min(latencies.len() - 1)]
    };
    (Some(pick(0.50)), Some(pick(0.95)))
}

/// Resolve a runnable claude executable. A child process can't invoke the npm `.cmd`/`.ps1`
/// shims with our quote-heavy args, so on Windows we prefer the real `claude.exe` the shim wraps.
fn resolve_claude_bin(given: &str) -> String {
    if given != "claude" {
        return given.to_string();
    }
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            let p = format!(
                "{appdata}\\npm\\node_modules\\@anthropic-ai\\claude-code\\bin\\claude.exe"
            );
            if std::path::Path::new(&p).exists() {
                return p;
            }
        }
    }
    given.to_string()
}

fn value_to_text(v: &Value) -> String {
    match v.as_str() {
        Some(s) => s.to_string(),
        None => v.to_string(),
    }
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn get<T: serde::de::DeserializeOwned>(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    path: &str,
) -> Result<T> {
    let mut req = http.get(format!("{}{}", cli.base, path));
    if let Some(k) = &cli.key {
        req = req.bearer_auth(k);
    }
    let resp = req.send()?;
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        anyhow::bail!("GET {path} -> HTTP {}: {text}", status.as_u16());
    }
    serde_json::from_str(&text).with_context(|| format!("decoding response from {path}"))
}

fn post(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    path: &str,
    body: &Value,
) -> Result<Value> {
    let mut req = http.post(format!("{}{}", cli.base, path)).json(body);
    if let Some(k) = &cli.key {
        req = req.bearer_auth(k);
    }
    let resp = req.send()?;
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        anyhow::bail!("POST {path} -> HTTP {}: {text}", status.as_u16());
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
}
