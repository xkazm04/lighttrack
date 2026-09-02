//! The promotion gate: whether pointing a label at a version is allowed, and why not.
//!
//! Split out of [`crate::prompts`] (which owns the registry's routes) because the gate is the part
//! with a *policy* in it, and the policy grew a second question. It used to ask only "did the run
//! that scored this version beat the baseline?". It now asks, first, **"did that run actually run
//! this version?"** — because a version-triggered run used to carry `{prompt_id, prompt_version}`
//! as provenance copied from its enqueue payload while generating from the target's stored
//! `system_prompt`, so a green run could certify content it never saw.
//!
//! The new evidence is [`RESOLVED_PROMPT_VERSION`], written only by the runner code that fetched
//! the registry content and handed it to the generator.

use lighttrack_core::{BenchmarkRun, RESOLVED_PROMPT_VERSION};
use serde_json::Value;

const EPS: f64 = 1e-9;

/// What the gate decided. A warning is not a soft block: promotion proceeds, and the operator is
/// told the run they promoted on could not have seen its target.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GateOutcome {
    Allow,
    AllowWithWarning(String),
    Block(String),
}

impl GateOutcome {
    /// The refusal reason, when this is a block.
    pub(crate) fn blocked(&self) -> Option<&str> {
        match self {
            GateOutcome::Block(r) => Some(r),
            _ => None,
        }
    }
    pub(crate) fn warning(&self) -> Option<&str> {
        match self {
            GateOutcome::AllowWithWarning(w) => Some(w),
            _ => None,
        }
    }
}

/// What the gate knows about the run that scored the version being promoted: its mean, and — when
/// the runner recorded one — the upper bound of the ~95% CI on that mean, plus the run's own
/// `status`. Extracted from the run rather than recomputed, so the gate and the runner cannot drift
/// apart into two different notions of "regressed".
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct GateEvidence {
    pub(crate) mean: Option<f64>,
    /// `report.ci95[1]`, the runner's own upper confidence bound on the mean.
    pub(crate) ci_upper: Option<f64>,
    /// `true` when the runner's verdict for that run was `regressed`.
    pub(crate) runner_regressed: bool,
    /// The run never covered its whole case set — cancelled by an operator, or halted by the
    /// per-run budget ceiling. Its mean is over the subset that happened to finish, which is not
    /// evidence about the version.
    pub(crate) incomplete: Option<&'static str>,
    /// The prompt version the run **generated with**, as recorded by the code that resolved it.
    /// `None` means the run never resolved a prompt at all.
    pub(crate) resolved_version: Option<u32>,
}

/// The regression gate that turns promotion into a measurable quality step.
///
/// - `force` overrides everything.
/// - **Resolution first.** When the linked benchmark names this prompt in a target's `prompt_ref`
///   (`resolvable`), the run must carry [`RESOLVED_PROMPT_VERSION`] equal to the version being
///   promoted. A run that reports no resolved version did not read the registry, and one reporting
///   a different number certified different content — either way it is not evidence about *this*
///   version, whatever its score says. This runs before the baseline check because a score is only
///   meaningful once we know what was scored.
/// - A benchmark with **no** `prompt_ref` cannot resolve anything, and blocking every such project
///   would break gates that work today for people who have not migrated. One release of advisory:
///   promotion proceeds with a warning that says the gate is scoring stored target content, not the
///   version. (Documented in `docs/CI_GATE.md`.)
/// - No `baseline` → nothing to compare against, allow.
/// - `baseline` set but no scored run yet → block (an unverified promotion defeats the gate).
/// - The runner already called the run `regressed` → block, quoting its verdict. The runner's
///   verdict is the significance-aware one (paired per-case where possible, family-wise corrected),
///   so honouring it keeps ONE definition of regression in the product.
/// - Otherwise, when the run recorded a confidence bound, block only when the whole interval sits
///   below the baseline — the same rule `stats::verdict` applies. **This is deliberately weaker than
///   a plain `mean < baseline` compare**: that blocked on a 0.001 dip inside the noise of a 3-case
///   run, a false positive on the one gate that stops a deploy. A real regression (a drop larger
///   than the run's own uncertainty) still blocks, and a *noisy* run is not waved through either —
///   a wide interval means the evidence is weak in both directions, and the fix is more cases.
/// - A run with no recorded interval (legacy, or `n < 2`) keeps the plain scalar compare, so the
///   `scalar_fallback` honesty of the small-n path is preserved rather than silently upgraded.
pub(crate) fn gate_promotion(
    latest: Option<GateEvidence>,
    baseline: Option<f64>,
    force: bool,
    promoting: u32,
    resolvable: bool,
) -> GateOutcome {
    if force {
        return GateOutcome::Allow;
    }
    if resolvable {
        if let Some(reason) = resolution_refusal(latest, promoting) {
            return GateOutcome::Block(reason);
        }
    }
    let advisory = (!resolvable).then(|| {
        format!(
            "promoted without a resolved-version check: the linked benchmark has no target with a \
             `prompt_ref`, so its runs generate from each target's stored `system_prompt` and \
             cannot certify version {promoting}'s content. Add a `prompt_ref` to the benchmark's \
             targets to gate on what actually ran."
        )
    });
    match regression_refusal(latest, baseline) {
        Some(reason) => GateOutcome::Block(reason),
        None => match advisory {
            Some(w) => GateOutcome::AllowWithWarning(w),
            None => GateOutcome::Allow,
        },
    }
}

/// The "did the run see its target?" half. `Some(reason)` refuses.
fn resolution_refusal(latest: Option<GateEvidence>, promoting: u32) -> Option<String> {
    let Some(ev) = latest else {
        return Some(format!(
            "promotion blocked: the linked benchmark has no run that scored version {promoting} \
             yet (run it before promoting, or pass force=true)"
        ));
    };
    match ev.resolved_version {
        Some(v) if v == promoting => None,
        Some(v) => Some(format!(
            "promotion blocked: the benchmark run backing this promotion generated with prompt \
             version {v}, not the version {promoting} being promoted — its score is evidence about \
             different content (re-run the benchmark for v{promoting}, or pass force=true)"
        )),
        None => Some(format!(
            "promotion blocked: the benchmark run backing this promotion reports no \
             `{RESOLVED_PROMPT_VERSION}`, so it never fetched version {promoting}'s content and \
             its score says nothing about it. Re-run the benchmark with a worker that resolves \
             prompt refs, or pass force=true."
        )),
    }
}

/// The "is the score good enough?" half, unchanged in policy. `Some(reason)` refuses.
fn regression_refusal(latest: Option<GateEvidence>, baseline: Option<f64>) -> Option<String> {
    let baseline = baseline?;
    let unscored = || {
        Some(
            "promotion blocked: linked benchmark has no scored run yet (run it before promoting, \
             or pass force=true)"
                .to_string(),
        )
    };
    // "No run at all" and "a run that recorded no mean" are the same fact to a gate: nothing has
    // been measured. Only reachable with a baseline set, which the `?` above established.
    let Some(ev) = latest else {
        return unscored();
    };
    let Some(mean) = ev.mean else {
        return unscored();
    };
    if ev.runner_regressed {
        return Some(format!(
            "promotion blocked: the benchmark run that scored this version reported status \
             'regressed' (mean {mean:.3} vs baseline {baseline:.3}) (pass force=true to override)"
        ));
    }
    // A cancelled or budget-halted run only scored the cases that finished before it stopped, and
    // WHICH cases those were is scheduling-dependent. Its mean is a fact about a subset, not about
    // the version — a favourable subset must never be able to promote. Block on the run's own
    // partiality rather than on its number.
    if let Some(why) = ev.incomplete {
        return Some(format!(
            "promotion blocked: the benchmark run that scored this version is incomplete ({why}) — \
             its mean {mean:.3} covers only the cases that ran (pass force=true to override, or \
             re-run the benchmark to completion)"
        ));
    }
    match ev.ci_upper {
        Some(upper) if upper + EPS < baseline => Some(format!(
            "promotion blocked: benchmark mean {mean:.3} (95% CI upper {upper:.3}) is significantly \
             below baseline {baseline:.3} (pass force=true to override)"
        )),
        Some(_) => None,
        // No interval recorded: fall back to the bare mean compare, as before.
        None if mean + EPS < baseline => Some(format!(
            "promotion blocked: benchmark mean {mean:.3} regressed below baseline {baseline:.3} \
             (no confidence interval recorded — scalar compare) (pass force=true to override)"
        )),
        None => None,
    }
}

/// The gate evidence from a run: its mean, the runner's own confidence bound, whether the runner
/// called it a regression, and which prompt version it actually generated with. Reading the
/// runner's numbers instead of re-deriving them is what keeps one definition of "regressed".
pub(crate) fn evidence_of(run: &BenchmarkRun) -> GateEvidence {
    GateEvidence {
        mean: run.mean_score,
        ci_upper: run
            .report
            .get("ci95")
            .and_then(Value::as_array)
            .and_then(|a| a.get(1))
            .and_then(Value::as_f64),
        runner_regressed: run.status == "regressed",
        // `cancelled` / `partial` are the run-control and budget-ceiling statuses; `aborted` is a
        // pre-flight refusal. None of them cover the full case set.
        incomplete: match run.status.as_str() {
            "cancelled" => Some("cancelled"),
            "partial" => Some("halted by the run budget ceiling"),
            "aborted" => Some("aborted"),
            _ => None,
        },
        resolved_version: run
            .report
            .get(RESOLVED_PROMPT_VERSION)
            .and_then(Value::as_u64)
            .map(|v| v as u32),
    }
}

/// The gate evidence from the most recent run that **provably scored `version` of `prompt_id`** —
/// its report carries the `{prompt_id, prompt_version}` the version-triggered enqueue stamped
/// through the runner. Runs are matched newest-`finished_at`-first. For benches whose runs predate
/// the tagging (no tagged run at all), falls back to the newest scored run of any version, so legacy
/// projects keep a working gate rather than an always-blocking one; once tagged runs exist for the
/// version, only they count — a tagged-but-unscored set correctly reads as "no scored run yet".
pub(crate) fn version_scored_run(
    runs: &[BenchmarkRun],
    prompt_id: &str,
    version: u32,
) -> Option<GateEvidence> {
    let mut tagged: Vec<&BenchmarkRun> = runs
        .iter()
        .filter(|r| {
            r.report.get("prompt_id").and_then(Value::as_str) == Some(prompt_id)
                && r.report.get("prompt_version").and_then(Value::as_u64) == Some(version as u64)
        })
        .collect();
    if tagged.is_empty() {
        return runs
            .iter()
            .find(|r| r.mean_score.is_some())
            .map(evidence_of);
    }
    tagged.sort_by_key(|r| r.finished_at);
    tagged
        .iter()
        .rev()
        .find(|r| r.mean_score.is_some())
        .map(|r| evidence_of(r))
}

#[cfg(test)]
#[path = "tests_prompt_gate.rs"]
mod tests;
