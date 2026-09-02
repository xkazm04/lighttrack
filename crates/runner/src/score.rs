//! `score` / `score-text`: judge stored events or an ad-hoc input/output pair.
//!
//! `score` is **online evaluation**: it judges recent events that carry input+output content,
//! skips events that already have a score (idempotent / re-runnable), and with `--interval` runs
//! as a continuous loop scoring newly-arrived events.
//!
//! Both commands judge under a [`Judge`] — freeform `--rubric` criteria or a structured
//! `--rubric-id` — resolved once by the caller, so the weighted-dimension methodology is reachable
//! from the primary scoring command and not only from `bench` / `score-traces` / `calibrate`.

use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};

use lighttrack_core::LlmEvent;
use lighttrack_engine::EngineConfig;

use crate::cli::Cli;
use crate::http::{get, post};
use crate::judge_spec::{Judge, Verdict};
use crate::util::{parallel_map, short, value_to_text};

/// Online scoring: judge recent unscored events (with input+output) for a project. With
/// `interval > 0`, loops forever, scoring newly-arrived events each cycle. `jobs` bounds how many
/// events are judged concurrently.
#[allow(clippy::too_many_arguments)]
pub(crate) fn score_recent(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    judge: &Judge,
    project: Option<&str>,
    prompt_tag: Option<&str>,
    limit: usize,
    interval: u64,
    jobs: usize,
) -> Result<()> {
    if interval > 0 {
        println!(
            "online scoring every {interval}s (judge={}, rubric='{}', limit={limit})",
            engine.model,
            judge.label()
        );
    }
    loop {
        score_once(cli, http, engine, judge, project, prompt_tag, limit, jobs)?;
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
#[allow(clippy::too_many_arguments)]
fn score_once(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    judge: &Judge,
    project: Option<&str>,
    prompt_tag: Option<&str>,
    limit: usize,
    jobs: usize,
) -> Result<usize> {
    // Ask the server for the unscored work list directly. This replaces the old client-side anti-join
    // that fetched the top-1000 scores and skipped events found among them — which silently re-judged
    // events (burning paid judge calls) once a project passed 1000 scores, and transferred up to 1000
    // full Score rows every interval tick. The server scopes the "already scored" check to exactly the
    // returned page's event ids, so it stays correct at any scale.
    let mut epath = format!("/v1/events?unscored=1&limit={limit}");
    if let Some(p) = project {
        epath.push_str(&format!("&project={p}"));
    }
    // Prioritise one served version'''s traffic (M23). The server does the narrowing, for the same
    // reason it does the anti-join: filtering a page client-side would return almost nothing on a
    // canary that is a few percent of the stream.
    if let Some(t) = prompt_tag {
        epath.push_str(&format!("&prompt={t}"));
    }
    let events: Vec<LlmEvent> = get(cli, http, &epath)?;

    // Partition first (cheap, in order): eligible events keep their (event, input, output); events
    // without both input and output content are skipped. Only the eligible set is judged.
    let total = events.len();
    let mut eligible: Vec<(&LlmEvent, String, String)> = Vec::new();
    let mut skipped = 0usize;
    for ev in &events {
        match (ev.input.as_ref(), ev.output.as_ref()) {
            (Some(i), Some(o)) => eligible.push((ev, value_to_text(i), value_to_text(o))),
            _ => skipped += 1,
        }
    }

    let judged: Vec<Result<Verdict>> = parallel_map(eligible.len(), jobs, |i| {
        let (_, input, output) = &eligible[i];
        judge.judge(engine, input, output)
    });

    let mut scored = 0usize;
    for (i, verdict) in judged.into_iter().enumerate() {
        let (ev, _, _) = &eligible[i];
        let mut v = verdict?;
        // The judge read the *stored* text, which the ingest scrub may already have rewritten. Copy
        // what the boundary did onto the verdict, so "why is this score odd" has an answer at the
        // verdict rather than only at the row.
        crate::provenance::stamp_evidence(&mut v.detail, ev);
        let score = build_score(&ev.project_id, Some(&ev.id), judge, &v);
        post(cli, http, "/v1/scores", &score)?;
        scored += 1;
        println!(
            "  - {} ({}) score={:.2}/{:.0} pass={} cost={} :: {}",
            short(&ev.id),
            ev.model,
            v.value,
            v.max,
            v.pass,
            v.cost_usd
                .map(|c| format!("${c:.5}"))
                .unwrap_or_else(|| "n/a".into()),
            v.reasoning
        );
    }
    println!(
        "scored {scored}, skipped {skipped} (already-scored or no content) of {total} fetched"
    );
    Ok(scored)
}

/// Score a single ad-hoc input/output pair.
pub(crate) fn score_text(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    judge: &Judge,
    input: &str,
    output: &str,
    project: &str,
) -> Result<()> {
    let v = judge.judge(engine, input, output)?;
    let score = build_score(project, None, judge, &v);
    let stored = post(cli, http, "/v1/scores", &score)?;
    println!("posted score: {}", serde_json::to_string_pretty(&stored)?);
    Ok(())
}

fn build_score(project_id: &str, event_id: Option<&str>, judge: &Judge, v: &Verdict) -> Value {
    json!({
        "project_id": project_id,
        "event_id": event_id,
        "rubric": judge.label(),
        // The typed identity beside the label. `freeform` is not a placeholder here — an ad-hoc
        // criteria string genuinely is a freeform verdict, and saying so keeps it out of the rollups
        // that should only average verdicts judged against a stored rubric.
        "rubric_id": judge.rubric_id(),
        "kind": judge.kind().as_str(),
        "value": v.value,
        "max": v.max,
        "pass": v.pass,
        "reasoning": v.reasoning,
        "detail": v.detail,
        "scored_by": v.scored_by,
        "cost_usd": v.cost_usd,
    })
}
