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
        score_once(cli, http, engine, judge, project, limit, jobs)?;
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
    judge: &Judge,
    project: Option<&str>,
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
    let events: Vec<LlmEvent> = get(cli, http, &epath)?;

    // Partition first (cheap, in order): eligible events keep their (event, input, output); events
    // without both input and output content are skipped. Only the eligible set is judged.
    let total = events.len();
    let mut eligible: Vec<(&LlmEvent, String, String)> = Vec::new();
    let mut skipped = 0usize;
    for ev in &events {
        match judgeable(ev) {
            Some((i, o)) => eligible.push((ev, i, o)),
            None => skipped += 1,
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

/// The one predicate that decides whether an event gets judged: it has both an input and an
/// output, rendered as the text the judge reads. Everything else is skipped.
///
/// Named rather than inlined because it is a *contract* other parts of the system have to satisfy
/// — M19's relay settle event exists to satisfy it. "No content ⇒ never scored" silently excluded
/// the one LLM workload LightTrack originates for as long as nobody wrote it down.
fn judgeable(ev: &LlmEvent) -> Option<(String, String)> {
    match (ev.input.as_ref(), ev.output.as_ref()) {
        (Some(i), Some(o)) => Some((value_to_text(i), value_to_text(o))),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A relay settle event as the cloud writes it, with and without the action's `report_io`.
    fn relay_event(with_content: bool) -> LlmEvent {
        let mut ev: LlmEvent = serde_json::from_value(json!({
            "project_id": "p", "provider": "anthropic", "model": "claude-code",
            "name": "relay-run", "tags": ["relay"],
            "metadata": { "action_type": "xprice/summary", "prompt_sha256": "ab" },
        }))
        .expect("event fixture");
        if with_content {
            ev.input = Some(json!("Price SKU A-1"));
            ev.output = Some(json!({ "text": "A-1 is $12" }));
        }
        ev
    }

    /// M19's whole claim, at the gate that used to refuse it: a relay run needs NO new scorer — it
    /// needed content. Without it the online scorer counts it as "no content" and skips it, which
    /// is how 100% of relay traffic went unjudged; with it the run is judged like any other call.
    #[test]
    fn a_relay_run_is_judged_once_its_action_reports_its_io() {
        assert!(
            judgeable(&relay_event(false)).is_none(),
            "an action that has not opted in stays unjudgeable, by design"
        );
        let (input, output) = judgeable(&relay_event(true)).expect("opted in ⇒ judged");
        assert_eq!(input, "Price SKU A-1");
        assert!(output.contains("A-1 is $12"), "{output}");
    }

    /// Half the pair is not a pair: judging an output against a missing prompt would produce a
    /// verdict about nothing.
    #[test]
    fn one_side_alone_is_never_enough() {
        let mut only_in = relay_event(true);
        only_in.output = None;
        assert!(judgeable(&only_in).is_none());
        let mut only_out = relay_event(true);
        only_out.input = None;
        assert!(judgeable(&only_out).is_none());
    }
}
