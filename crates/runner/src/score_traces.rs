//! `score-traces`: an auto-scoring policy loop that judges *whole traces* so they score themselves
//! without anyone at the keyboard.
//!
//! Each cycle walks the project's recently-*completed* traces newest-first (via the `/v1/traces`
//! keyset window from the trace-list filters), takes a stable 1/N sample (plus every error trace
//! when `--errors-always`), judges each sampled trace's **root exchange** (the root span's
//! input/output — the whole-request in/out), and posts a whole-trace score anchored to the root.
//!
//! "Completed" is approximated: traces carry no explicit end marker, so a trace counts as done once
//! its newest event is older than a settle window (`--settle-secs`, default 120s) — long enough that
//! a still-streaming request won't be judged mid-flight. The pass is **idempotent**: a trace that
//! already has a whole-trace score for this rubric is skipped, so a daemon or a cron `--once` run
//! never double-scores. The judge is unbudgeted and uses only existing endpoints.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};

use lighttrack_core::{Rubric, TraceSummary};
use lighttrack_engine::{
    build_judge_prompt, parse_judge_spec, run_judge, run_rubric_judge, EngineConfig,
};

use crate::cli::Cli;
use crate::http::{get, get_paged, post};
use crate::util::{parallel_map, short, value_to_text};

/// Everything one `score-traces` invocation needs (borrows the parsed CLI strings).
pub(crate) struct Params<'a> {
    pub project: &'a str,
    pub rubric_text: Option<&'a str>,
    pub rubric_id: Option<&'a str>,
    pub sample_every: usize,
    pub errors_always: bool,
    pub settle_secs: i64,
    pub limit: usize,
    pub interval: u64,
    pub once: bool,
    pub jobs: usize,
}

/// The judging contract for this run: freeform criteria, or a fetched structured rubric. The
/// `label` is the `rubric` field written on every score — and the key the idempotency check matches.
enum Judge {
    Freeform(String),
    Structured(Box<Rubric>),
}

impl Judge {
    fn label(&self) -> &str {
        match self {
            Judge::Freeform(text) => text,
            Judge::Structured(r) => &r.name,
        }
    }
}

/// A unified judge verdict, whichever mode produced it — the shape the score body needs.
struct Verdict {
    value: f64,
    max: f64,
    pass: bool,
    reasoning: String,
    scored_by: String,
    cost_usd: Option<f64>,
}

/// A sampled trace whose root exchange is ready to judge.
struct Eligible {
    trace_id: String,
    input: String,
    output: String,
}

pub(crate) fn run(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    p: &Params,
) -> Result<()> {
    let judge = resolve_judge(cli, http, p)?;
    let daemon = p.interval > 0 && !p.once;
    if daemon {
        println!(
            "auto-scoring traces for '{}' every {}s (settle={}s, sample_every={}, errors_always={}, judge={})",
            p.project, p.interval, p.settle_secs, p.sample_every, p.errors_always, engine.model
        );
    }
    loop {
        match run_cycle(cli, http, engine, p, &judge) {
            Ok(n) => println!("cycle: scored {n} trace(s)"),
            // A daemon must survive a transient failure (API briefly down); a one-shot run propagates
            // it so a cron/scheduler step fails loudly.
            Err(e) if daemon => eprintln!("cycle error (continuing): {e}"),
            Err(e) => return Err(e),
        }
        if !daemon {
            break;
        }
        std::thread::sleep(Duration::from_secs(p.interval));
    }
    Ok(())
}

/// Resolve the judging contract; exactly one of `--rubric` / `--rubric-id` is required.
fn resolve_judge(cli: &Cli, http: &reqwest::blocking::Client, p: &Params) -> Result<Judge> {
    match (p.rubric_text, p.rubric_id) {
        (Some(t), None) => Ok(Judge::Freeform(t.to_string())),
        (None, Some(id)) => {
            let r: Rubric = get(cli, http, &format!("/v1/rubrics/{id}"))
                .with_context(|| format!("fetching rubric '{id}'"))?;
            Ok(Judge::Structured(Box::new(r)))
        }
        (Some(_), Some(_)) => bail!("pass exactly one of --rubric or --rubric-id, not both"),
        (None, None) => bail!("one of --rubric or --rubric-id is required"),
    }
}

/// One policy pass: sample settled traces, judge each eligible root exchange, post the scores.
fn run_cycle(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    p: &Params,
    judge: &Judge,
) -> Result<usize> {
    let label = judge.label();
    // Traces whose newest event is older than the settle window count as completed.
    let cutoff = (Utc::now() - chrono::Duration::seconds(p.settle_secs))
        .to_rfc3339_opts(SecondsFormat::Nanos, true);

    // Fetch each sampled trace's detail; keep those that still need a judge (not already scored for
    // this rubric, and whose root span carries an output to judge).
    let mut eligible: Vec<Eligible> = Vec::new();
    for ts in collect_sampled(cli, http, p, &cutoff)? {
        let detail: Value = get(cli, http, &format!("/v1/traces/{}", ts.trace_id))?;
        let root = detail.pointer("/spans/0/event");
        let root_id = root.and_then(|e| e.get("id")).and_then(Value::as_str).unwrap_or("");
        let already = trace_already_scored(&detail, label, root_id);
        // Final gate through the same pure decision, now with the real already-scored flag.
        if !should_score(&ts.trace_id, ts.status == "error", p.sample_every, p.errors_always, already)
        {
            continue;
        }
        // The root exchange: judge the whole request's in/out. Skip a trace whose root has no output.
        let output = match root.and_then(|e| text_field(e, "output")) {
            Some(o) => o,
            None => continue,
        };
        let input = root.and_then(|e| text_field(e, "input")).unwrap_or_default();
        eligible.push(Eligible { trace_id: ts.trace_id, input, output });
    }

    // Judge concurrently (unbudgeted, read-only); post in fetch order so output is deterministic.
    let judged: Vec<Result<Verdict>> = parallel_map(eligible.len(), p.jobs, |i| {
        judge_one(engine, judge, &eligible[i].input, &eligible[i].output)
    });
    let mut scored = 0usize;
    for (i, verdict) in judged.into_iter().enumerate() {
        let e = &eligible[i];
        let v = verdict?;
        let body = json!({
            "rubric": label, "value": v.value, "max": v.max, "pass": v.pass,
            "reasoning": v.reasoning, "scored_by": v.scored_by, "cost_usd": v.cost_usd,
        });
        post(cli, http, &format!("/v1/traces/{}/score", e.trace_id), &body)?;
        scored += 1;
        println!(
            "  - trace {} score={:.2}/{:.0} pass={} :: {}",
            short(&e.trace_id),
            v.value,
            v.max,
            v.pass,
            v.reasoning.chars().take(80).collect::<String>()
        );
    }
    Ok(scored)
}

/// Walk settled traces newest-ended-first through the keyset window, cheap-filtering (already-scored
/// unknown here → `false`) down to the sample. Follows `X-Next-Cursor` until `limit` traces have been
/// considered or the pages run out.
fn collect_sampled(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    p: &Params,
    cutoff: &str,
) -> Result<Vec<TraceSummary>> {
    let mut out = Vec::new();
    let mut considered = 0usize;
    let mut cursor: Option<String> = None;
    let page = p.limit.clamp(1, 200);
    while considered < p.limit {
        let mut path =
            format!("/v1/traces?project={}&until={}&limit={}", p.project, cutoff, page);
        if let Some(c) = &cursor {
            path.push_str(&format!("&cursor={c}"));
        }
        let (traces, next): (Vec<TraceSummary>, Option<String>) = get_paged(cli, http, &path)?;
        if traces.is_empty() {
            break;
        }
        for t in traces {
            if considered >= p.limit {
                break;
            }
            considered += 1;
            if should_score(&t.trace_id, t.status == "error", p.sample_every, p.errors_always, false)
            {
                out.push(t);
            }
        }
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    Ok(out)
}

/// Judge one root exchange under the run's contract, mapping either outcome to a [`Verdict`].
fn judge_one(engine: &EngineConfig, judge: &Judge, input: &str, output: &str) -> Result<Verdict> {
    let (jp, jm) = parse_judge_spec(&engine.model);
    match judge {
        Judge::Freeform(text) => {
            let prompt = build_judge_prompt(text, input, output);
            let o = run_judge(engine, &jp, &jm, &prompt).context("judge failed")?;
            Ok(Verdict {
                value: o.verdict.score,
                max: o.verdict.max,
                pass: o.verdict.pass,
                reasoning: o.verdict.reasoning,
                scored_by: o.model,
                cost_usd: o.cost_usd,
            })
        }
        Judge::Structured(r) => {
            let o = run_rubric_judge(engine, &jp, &jm, r, input, None, output, 1, 1)
                .context("rubric judge failed")?;
            Ok(Verdict {
                value: o.overall,
                max: 1.0,
                pass: o.pass,
                reasoning: format!("rubric '{}' overall over {} dims", r.name, o.dimensions.len()),
                scored_by: o.model,
                cost_usd: o.cost_usd,
            })
        }
    }
}

/// A trace already carries a whole-trace score for this rubric iff one of its scores has our
/// `rubric` label anchored to the root event (the whole-trace anchor `score-traces` posts to).
fn trace_already_scored(detail: &Value, label: &str, root_id: &str) -> bool {
    detail
        .get("scores")
        .and_then(Value::as_array)
        .map(|scores| {
            scores.iter().any(|s| {
                s.get("rubric").and_then(Value::as_str) == Some(label)
                    && s.get("event_id").and_then(Value::as_str) == Some(root_id)
            })
        })
        .unwrap_or(false)
}

/// Pull a span-event text field (`input`/`output`) as plain text, treating a missing or `null`
/// value as absent.
fn text_field(event: &Value, key: &str) -> Option<String> {
    match event.get(key) {
        Some(v) if !v.is_null() => Some(value_to_text(v)),
        _ => None,
    }
}

/// Pure sampling policy: should this trace be judged this cycle?
///
/// - An already-scored trace (for this rubric) is never re-judged → idempotent.
/// - With `errors_always`, every error trace is judged regardless of the sample rate.
/// - Otherwise the trace is in the sample iff a stable hash of its id falls in the 1/`sample_every`
///   bucket — order-independent, so the same ~1/N subset is chosen each cycle (`sample_every` ≤ 1 =
///   every trace).
pub(crate) fn should_score(
    trace_id: &str,
    is_error: bool,
    sample_every: usize,
    errors_always: bool,
    already_scored: bool,
) -> bool {
    if already_scored {
        return false;
    }
    if errors_always && is_error {
        return true;
    }
    let n = sample_every.max(1) as u64;
    fnv1a(trace_id).is_multiple_of(n)
}

/// FNV-1a 64-bit — a small, stable, dependency-free hash so the 1/N sample is reproducible across
/// processes (unlike `DefaultHasher`, whose output isn't guaranteed stable).
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn already_scored_traces_are_never_rejudged() {
        // Regardless of sampling/errors, an already-scored trace is skipped → repeated runs never
        // double-score.
        assert!(!should_score("t", false, 1, false, true));
        assert!(!should_score("t", true, 1, true, true));
    }

    #[test]
    fn errors_always_judges_every_error_trace() {
        // An error trace that wouldn't be in the hash sample is still judged with --errors-always...
        let big_n = 1_000_000;
        assert!(should_score("some-error-trace", true, big_n, true, false));
        // ...but a success trace outside the sample is not.
        let outside = (0..).map(|i| format!("s-{i}")).find(|id| !fnv1a(id).is_multiple_of(big_n as u64)).unwrap();
        assert!(!should_score(&outside, false, big_n, true, false));
        // And without --errors-always, an out-of-sample error trace is not judged either.
        let err_outside =
            (0..).map(|i| format!("e-{i}")).find(|id| !fnv1a(id).is_multiple_of(big_n as u64)).unwrap();
        assert!(!should_score(&err_outside, true, big_n, false, false));
    }

    #[test]
    fn sample_every_one_scores_all_and_is_a_stable_fraction() {
        // sample_every <= 1 → every (unscored) trace.
        for i in 0..50 {
            assert!(should_score(&format!("t-{i}"), false, 1, false, false));
            assert!(should_score(&format!("t-{i}"), false, 0, false, false));
        }
        // A coarse 1/4 sample keeps a strict subset — not none, not all — and is deterministic.
        let ids: Vec<String> = (0..400).map(|i| format!("trace-{i}")).collect();
        let picked = ids.iter().filter(|id| should_score(id, false, 4, false, false)).count();
        assert!(picked > 0 && picked < ids.len(), "1/4 sample picked {picked}/400");
        // Same input → same decision (stable across "cycles").
        assert_eq!(
            should_score("trace-7", false, 4, false, false),
            should_score("trace-7", false, 4, false, false)
        );
    }

    #[test]
    fn already_scored_matches_rubric_and_root_anchor() {
        let detail = json!({
            "scores": [
                { "rubric": "helpfulness", "event_id": "root-1" },
                { "rubric": "other",       "event_id": "root-1" },
                { "rubric": "helpfulness", "event_id": "child-9" }
            ]
        });
        // Whole-trace score for this rubric, anchored to the root → already scored.
        assert!(trace_already_scored(&detail, "helpfulness", "root-1"));
        // A per-call score with the same rubric on a *child* span does not count as whole-trace.
        assert!(!trace_already_scored(&detail, "faithfulness", "root-1"));
        assert!(!trace_already_scored(&json!({}), "helpfulness", "root-1"));
    }

    #[test]
    fn text_field_treats_null_and_missing_as_absent() {
        let ev = json!({ "input": "hi", "output": null });
        assert_eq!(text_field(&ev, "input").as_deref(), Some("hi"));
        assert_eq!(text_field(&ev, "output"), None);
        assert_eq!(text_field(&ev, "missing"), None);
        // Non-string content is rendered as compact JSON.
        assert_eq!(text_field(&json!({ "input": { "q": 1 } }), "input").as_deref(), Some(r#"{"q":1}"#));
    }
}
