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

use lighttrack_core::LlmEvent;
use lighttrack_engine::{build_judge_prompt, run_judge, EngineConfig};

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
    }
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
