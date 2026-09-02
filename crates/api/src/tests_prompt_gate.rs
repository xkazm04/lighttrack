//! Tests for the promotion gate ([`crate::prompts_gate`]).
//!
//! Kept beside the policy rather than inside it: the gate is ~200 lines of rules and the cases that
//! pin them run longer than the rules themselves.

use crate::prompts_gate::*;

use chrono::Utc;
use lighttrack_core::{new_id, BenchmarkRun, RESOLVED_PROMPT_VERSION};
use serde_json::Value;

fn run_with(report: Value, mean: Option<f64>, finished_offset_secs: i64) -> BenchmarkRun {
    BenchmarkRun {
        id: new_id(),
        benchmark_id: "b".into(),
        started_at: Utc::now(),
        finished_at: Some(Utc::now() + chrono::Duration::seconds(finished_offset_secs)),
        n_cases: 1,
        mean_score: mean,
        pass_rate: mean,
        cost_usd: 0.0,
        status: "passed".into(),
        p50_latency_ms: None,
        p95_latency_ms: None,
        total_tokens: None,
        report,
    }
}

/// Legacy-shaped evidence: a bare mean with no recorded interval (the scalar-compare path).
fn scalar(mean: f64) -> Option<GateEvidence> {
    Some(GateEvidence {
        mean: Some(mean),
        ..Default::default()
    })
}

/// The same, plus a resolved version — what a post-M10 run looks like.
fn resolved(mean: f64, version: u32) -> Option<GateEvidence> {
    Some(GateEvidence {
        mean: Some(mean),
        resolved_version: Some(version),
        ..Default::default()
    })
}

/// Legacy call shape: no `prompt_ref` on the benchmark (advisory mode).
fn gate(latest: Option<GateEvidence>, baseline: Option<f64>, force: bool) -> GateOutcome {
    gate_promotion(latest, baseline, force, 9, false)
}

#[test]
fn gate_reads_the_run_for_the_promoted_version_not_the_newest() {
    let tag = |v: u32| serde_json::json!({ "prompt_id": "p1", "prompt_version": v });
    let mean_of = |e: Option<GateEvidence>| e.and_then(|e| e.mean);
    let runs = vec![
        // Newest run overall scored v3 GREEN — must NOT clear a v9 promotion.
        run_with(tag(3), Some(0.95), 100),
        // The run that actually scored v9 is older and RED.
        run_with(tag(9), Some(0.40), 50),
    ];
    assert_eq!(
        mean_of(version_scored_run(&runs, "p1", 9)),
        Some(0.40),
        "v9's own run counts"
    );
    assert_eq!(mean_of(version_scored_run(&runs, "p1", 3)), Some(0.95));
    // Two runs for the same version: the newest finished_at wins.
    let runs2 = vec![
        run_with(tag(9), Some(0.40), 10),
        run_with(tag(9), Some(0.90), 20),
    ];
    assert_eq!(mean_of(version_scored_run(&runs2, "p1", 9)), Some(0.90));
    // Tagged runs exist but none scored → None (the gate blocks as "no scored run yet").
    let runs3 = vec![run_with(tag(9), None, 10)];
    assert!(version_scored_run(&runs3, "p1", 9).is_none());
    // Legacy: no tagged runs at all → newest scored run of any version (old behavior preserved).
    let legacy = vec![run_with(Value::Null, Some(0.7), 10)];
    assert_eq!(mean_of(version_scored_run(&legacy, "p1", 9)), Some(0.7));
    // A different prompt's tag never matches.
    let other = vec![run_with(
        serde_json::json!({"prompt_id":"px","prompt_version":9}),
        Some(0.9),
        10,
    )];
    assert_eq!(
        mean_of(version_scored_run(&other, "p1", 9)),
        Some(0.9),
        "falls back to legacy path"
    );
}

#[test]
fn gate_allows_when_no_baseline_or_forced() {
    assert!(
        gate(scalar(0.1), None, false).blocked().is_none(),
        "no baseline → allow"
    );
    assert_eq!(
        gate_promotion(None, Some(0.9), true, 9, true),
        GateOutcome::Allow,
        "force overrides a block — including the resolution check"
    );
    assert!(gate(scalar(0.1), Some(0.9), true).blocked().is_none());
}

#[test]
fn gate_blocks_regression_and_unscored() {
    assert!(
        gate(None, Some(0.8), false).blocked().is_some(),
        "baseline but no run → block"
    );
    assert!(
        gate(scalar(0.79), Some(0.8), false).blocked().is_some(),
        "below baseline → block"
    );
    assert!(
        gate(scalar(0.8), Some(0.8), false).blocked().is_none(),
        "meeting baseline → allow"
    );
    assert!(gate(scalar(0.95), Some(0.8), false).blocked().is_none());
    // A run whose mean is missing entirely reads as "no scored run yet", not as a pass.
    let no_mean = Some(GateEvidence::default());
    assert!(gate(no_mean, Some(0.8), false).blocked().is_some());
}

#[test]
fn gate_is_significance_aware_when_the_run_recorded_an_interval() {
    // The false positive the old scalar gate produced: mean 0.79 vs baseline 0.80 on a noisy
    // run whose 95% interval reaches 0.88. That 0.01 dip is inside the run's own uncertainty,
    // so it is not evidence of a regression and must not block a deploy.
    let noisy = Some(GateEvidence {
        mean: Some(0.79),
        ci_upper: Some(0.88),
        ..Default::default()
    });
    assert!(
        gate(noisy, Some(0.80), false).blocked().is_none(),
        "a dip inside the noise"
    );
    // A REAL regression — the whole interval below baseline — still blocks.
    let real = Some(GateEvidence {
        mean: Some(0.50),
        ci_upper: Some(0.56),
        ..Default::default()
    });
    let reason = gate(real, Some(0.80), false).blocked().unwrap().to_string();
    assert!(reason.contains("significantly below"), "got: {reason}");
    assert!(
        reason.contains("0.560"),
        "the interval is quoted so the operator can check it"
    );
}

#[test]
fn gate_honours_the_runners_own_regressed_verdict() {
    let ev = Some(GateEvidence {
        mean: Some(0.85),
        ci_upper: Some(0.92),
        runner_regressed: true,
        ..Default::default()
    });
    let reason = gate(ev, Some(0.80), false).blocked().unwrap().to_string();
    assert!(reason.contains("'regressed'"), "got: {reason}");
    assert!(gate(ev, Some(0.80), true).blocked().is_none());
}

#[test]
fn an_incomplete_run_cannot_promote_however_good_its_mean_looks() {
    for (status, needle) in [
        ("cancelled", "cancelled"),
        ("partial", "budget"),
        ("aborted", "aborted"),
    ] {
        let mut run = run_with(serde_json::json!({}), Some(0.95), 1);
        run.status = status.to_string();
        let out = gate(Some(evidence_of(&run)), Some(0.80), false);
        let reason = out
            .blocked()
            .unwrap_or_else(|| panic!("{status} must not promote"));
        assert!(reason.contains("incomplete"), "got: {reason}");
        assert!(reason.contains(needle), "reason must name why: {reason}");
        assert!(gate(Some(evidence_of(&run)), Some(0.80), true)
            .blocked()
            .is_none());
    }
    let ok = run_with(serde_json::json!({}), Some(0.95), 1);
    assert!(gate(Some(evidence_of(&ok)), Some(0.80), false)
        .blocked()
        .is_none());
}

#[test]
fn a_resolvable_benchmark_refuses_a_run_that_never_read_the_registry() {
    // THE M10 failure: a run whose score is excellent and whose provenance tags say v9, but
    // which generated from the target's stored system_prompt. Before, this promoted.
    let out = gate_promotion(scalar(0.99), Some(0.80), false, 9, true);
    let reason = out.blocked().expect("must block");
    assert!(reason.contains(RESOLVED_PROMPT_VERSION), "got: {reason}");
    // A run that DID resolve v9 promotes.
    assert_eq!(
        gate_promotion(resolved(0.99, 9), Some(0.80), false, 9, true),
        GateOutcome::Allow
    );
    // A run that resolved a DIFFERENT version is evidence about other content.
    let reason = gate_promotion(resolved(0.99, 3), Some(0.80), false, 9, true)
        .blocked()
        .expect("must block")
        .to_string();
    assert!(reason.contains("version 3"), "got: {reason}");
    assert!(reason.contains("v9"), "got: {reason}");
    // Resolution is checked BEFORE the score, and with no baseline at all — a gate that only
    // engages once someone sets a baseline is not a gate on what ran.
    assert!(gate_promotion(scalar(0.99), None, false, 9, true)
        .blocked()
        .is_some());
    // …and a resolved run with no baseline is allowed, as before.
    assert_eq!(
        gate_promotion(resolved(0.10, 9), None, false, 9, true),
        GateOutcome::Allow
    );
}

#[test]
fn a_benchmark_with_no_prompt_ref_warns_rather_than_blocking_for_one_release() {
    // Existing projects have benchmarks whose targets carry a literal system_prompt. Blocking
    // them all would break working gates, so they promote with the honest caveat attached.
    let out = gate_promotion(scalar(0.99), Some(0.80), false, 9, false);
    let w = out.warning().expect("advisory, not a block");
    assert!(w.contains("prompt_ref"), "the warning says how to fix it");
    assert!(out.blocked().is_none());
    // The score rules still apply underneath the advisory — a regression is still a block.
    assert!(gate_promotion(scalar(0.10), Some(0.80), false, 9, false)
        .blocked()
        .is_some());
}

#[test]
fn evidence_reads_the_resolved_version_off_the_report() {
    let run = run_with(
        serde_json::json!({ RESOLVED_PROMPT_VERSION: 7, "prompt_version": 9 }),
        Some(0.5),
        1,
    );
    let ev = evidence_of(&run);
    assert_eq!(
        ev.resolved_version,
        Some(7),
        "the RESOLVED version is read, never the provenance tag beside it — that tag is exactly \
         what could lie"
    );
    assert_eq!(
        evidence_of(&run_with(Value::Null, None, 1)).resolved_version,
        None
    );
}
