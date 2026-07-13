//! `score` / `score-text`: judge stored events or an ad-hoc input/output pair.
//!
//! `score` is **online evaluation**: it judges recent events that carry input+output content,
//! skips events that already have a score (idempotent / re-runnable), and with `--interval` runs
//! as a continuous loop scoring newly-arrived events.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use lighttrack_core::{LlmEvent, Score};
use lighttrack_engine::{build_judge_prompt, parse_judge_spec, run_judge, EngineConfig, JudgeOutcome};

use crate::cli::Cli;
use crate::http::{get, post};
use crate::util::{parallel_map, short, value_to_text};

/// Online scoring: judge recent unscored events (with input+output) for a project. With
/// `interval > 0`, loops forever, scoring newly-arrived events each cycle. `jobs` bounds how many
/// events are judged concurrently.
#[allow(clippy::too_many_arguments)]
pub(crate) fn score_recent(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    rubric: &str,
    project: Option<&str>,
    limit: usize,
    interval: u64,
    jobs: usize,
) -> Result<()> {
    if interval > 0 {
        println!("online scoring every {interval}s (judge={}, limit={limit})", engine.model);
    }
    loop {
        score_once(cli, http, engine, rubric, project, limit, jobs)?;
        if interval == 0 {
            break;
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
    Ok(())
}

/// One scoring pass: judge recent events that carry content and aren't already scored. Eligible
/// events are judged with up to `jobs` concurrency; results are posted/printed in fetch order so the
/// output is deterministic (identical at any `jobs`). Returns the number newly scored.
fn score_once(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    rubric: &str,
    project: Option<&str>,
    limit: usize,
    jobs: usize,
) -> Result<usize> {
    let mut epath = format!("/v1/events?limit={limit}");
    let mut spath = "/v1/scores?limit=1000".to_string();
    if let Some(p) = project {
        epath.push_str(&format!("&project={p}"));
        spath.push_str(&format!("&project={p}"));
    }
    let events: Vec<LlmEvent> = get(cli, http, &epath)?;
    // Already-scored event ids → skip, so re-runs / the online loop don't re-judge.
    let scored_ids: HashSet<String> = get::<Vec<Score>>(cli, http, &spath)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|s| s.event_id)
        .collect();

    let (jp, jm) = parse_judge_spec(&engine.model);
    // Partition first (cheap, in order): eligible events keep their (event, input, output); the rest
    // are skipped. Only the eligible set is judged — and that judging is what we parallelize.
    let total = events.len();
    let mut eligible: Vec<(&LlmEvent, String, String)> = Vec::new();
    let mut skipped = 0usize;
    for ev in &events {
        match (scored_ids.contains(&ev.id), ev.input.as_ref(), ev.output.as_ref()) {
            (false, Some(i), Some(o)) => eligible.push((ev, value_to_text(i), value_to_text(o))),
            _ => skipped += 1,
        }
    }

    let judged: Vec<Result<JudgeOutcome>> = parallel_map(eligible.len(), jobs, |i| {
        let (_, input, output) = &eligible[i];
        judge_one(engine, &jp, &jm, rubric, input, output)
    });

    let mut scored = 0usize;
    for (i, outcome) in judged.into_iter().enumerate() {
        let (ev, _, _) = &eligible[i];
        let outcome = outcome?;
        let score = build_score(&ev.project_id, Some(&ev.id), rubric, &outcome);
        post(cli, http, "/v1/scores", &score)?;
        scored += 1;
        println!(
            "  - {} ({}) score={:.2}/{:.0} pass={} cost={} :: {}",
            short(&ev.id),
            ev.model,
            outcome.verdict.score,
            outcome.verdict.max,
            outcome.verdict.pass,
            outcome.cost_usd.map(|c| format!("${c:.5}")).unwrap_or_else(|| "n/a".into()),
            outcome.verdict.reasoning
        );
    }
    println!("scored {scored}, skipped {skipped} (already-scored or no content) of {total} fetched");
    Ok(scored)
}

/// Score a single ad-hoc input/output pair.
pub(crate) fn score_text(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    rubric: &str,
    input: &str,
    output: &str,
    project: &str,
) -> Result<()> {
    let (jp, jm) = parse_judge_spec(&engine.model);
    let outcome = judge_one(engine, &jp, &jm, rubric, input, output)?;
    let score = build_score(project, None, rubric, &outcome);
    let stored = post(cli, http, "/v1/scores", &score)?;
    println!("posted score: {}", serde_json::to_string_pretty(&stored)?);
    Ok(())
}

fn judge_one(
    engine: &EngineConfig,
    provider: &str,
    model: &str,
    rubric: &str,
    input: &str,
    output: &str,
) -> Result<JudgeOutcome> {
    let prompt = build_judge_prompt(rubric, input, output);
    run_judge(engine, provider, model, &prompt).context("judge failed")
}

fn build_score(
    project_id: &str,
    event_id: Option<&str>,
    rubric: &str,
    outcome: &JudgeOutcome,
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
