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
use crate::util::{short, value_to_text};

/// Online scoring: judge recent unscored events (with input+output) for a project. With
/// `interval > 0`, loops forever, scoring newly-arrived events each cycle.
pub(crate) fn score_recent(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    rubric: &str,
    project: Option<&str>,
    limit: usize,
    interval: u64,
) -> Result<()> {
    if interval > 0 {
        println!("online scoring every {interval}s (judge={}, limit={limit})", engine.model);
    }
    loop {
        score_once(cli, http, engine, rubric, project, limit)?;
        if interval == 0 {
            break;
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
    Ok(())
}

/// One scoring pass: judge recent events that carry content and aren't already scored. Returns the
/// number newly scored.
fn score_once(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    rubric: &str,
    project: Option<&str>,
    limit: usize,
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
    let (mut scored, mut skipped) = (0usize, 0usize);
    for ev in &events {
        if scored_ids.contains(&ev.id) {
            skipped += 1;
            continue;
        }
        let (input, output) = match (ev.input.as_ref(), ev.output.as_ref()) {
            (Some(i), Some(o)) => (value_to_text(i), value_to_text(o)),
            _ => {
                skipped += 1;
                continue;
            }
        };
        print!("  - judging {} ({})... ", short(&ev.id), ev.model);
        let outcome = judge_one(engine, &jp, &jm, rubric, &input, &output)?;
        let score = build_score(&ev.project_id, Some(&ev.id), rubric, &outcome);
        post(cli, http, "/v1/scores", &score)?;
        scored += 1;
        println!(
            "score={:.2}/{:.0} pass={} cost={} :: {}",
            outcome.verdict.score,
            outcome.verdict.max,
            outcome.verdict.pass,
            outcome.cost_usd.map(|c| format!("${c:.5}")).unwrap_or_else(|| "n/a".into()),
            outcome.verdict.reasoning
        );
    }
    println!(
        "scored {scored}, skipped {skipped} (already-scored or no content) of {} fetched",
        events.len()
    );
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
