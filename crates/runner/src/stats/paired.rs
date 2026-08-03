//! The verdict math behind "B beats A": a **paired** per-case test against the previous run, the
//! absolute-floor test against the benchmark's `baseline_score`, and the family-wise correction that
//! keeps a six-target comparison from tripping a false `regressed` a quarter of the time.
//!
//! Why paired: the same cases are judged in both runs, so the per-case *difference* removes
//! between-case variance entirely. An unpaired comparison of two means over hard-and-easy cases is
//! dominated by how hard the cases are; the paired one measures only what changed. That is
//! typically several times more power at the same sample size, which is the difference between
//! "we cannot tell" and a usable gate.

use serde_json::{json, Value};

use super::normal::{bonferroni_alpha, bonferroni_z, two_sided_p};
use super::{Summary, EPS};

/// Family-wise significance level: the probability that *any* of a run's comparisons produces a
/// false `regressed`. Fixed rather than configurable — a benchmark tool whose confidence level is a
/// knob invites tuning it until the answer is the desired one.
pub(crate) const ALPHA: f64 = 0.05;

/// A run's verdict, with the evidence that produced it. Every field is reported, so a reader can
/// see *which* test decided and how much was left of α after the correction.
#[derive(Debug, Clone)]
pub(crate) struct SigVerdict {
    /// `regressed` | `passed` | `no_baseline` — the unchanged run-status vocabulary.
    pub(crate) status: &'static str,
    /// The strongest test that ran: `paired-z`, `unpaired-ci`, `scalar`, or `none`.
    pub(crate) method: &'static str,
    /// The n<2 path, where there is no stderr and we fall back to a plain scalar compare.
    pub(crate) scalar_fallback: bool,
    /// Per-comparison α left after the family-wise correction.
    pub(crate) alpha: f64,
    /// Family size the correction was applied over.
    pub(crate) comparisons: usize,
    /// Two-sided p of the paired test, when one ran.
    pub(crate) p_value: Option<f64>,
    /// Mean per-case change vs the previous run, when pairing was possible.
    pub(crate) mean_delta: Option<f64>,
    /// Honest limitations of this particular verdict, in plain words.
    pub(crate) caveats: Vec<String>,
}

/// Per-case differences `run[i] − baseline[i]`. `None` — never a silent truncation — when the case
/// sets don't line up: a paired test over mismatched cases is worse than no paired test at all.
pub(crate) fn paired_deltas(run: &[f64], baseline: &[f64]) -> Option<Vec<f64>> {
    if run.is_empty() || run.len() != baseline.len() {
        return None;
    }
    Some(run.iter().zip(baseline).map(|(a, b)| a - b).collect())
}

/// One-sample z on the per-case deltas: `z = mean(Δ) / stderr(Δ)`, with its two-sided p.
/// `None` when there is no spread to test against (`n < 2`).
///
/// A zero stderr with a non-zero mean (every case moved by the same amount) is a *perfectly*
/// consistent change, so it reports an infinite z and p = 0 rather than being discarded.
pub(crate) fn paired_z(deltas: &[f64]) -> Option<(f64, f64, f64)> {
    let s = Summary::of(deltas);
    if s.n < 2 {
        return None;
    }
    let z = if s.stderr > 0.0 {
        s.mean / s.stderr
    } else if s.mean.abs() > EPS {
        f64::INFINITY * s.mean.signum()
    } else {
        0.0
    };
    Some((s.mean, z, two_sided_p(z)))
}

/// The run verdict. `baseline` is the benchmark's absolute floor (`baseline_score`); `s` summarizes
/// this run's per-case scores; `deltas` are the per-case changes vs the previous comparable run, when
/// one was found; `m` is the family size for the correction (targets in a compare run, else 1).
///
/// Two tests, deliberately composed so the change can only *add* detection, never remove it:
/// 1. **Absolute floor** — the existing CI-excludes-baseline test, now at the corrected critical z.
/// 2. **Paired drop** — a significant negative mean delta vs the previous run.
///
/// `regressed` if either fires. The paired test only *gates* when a `baseline_score` is set: without
/// one the benchmark has opted out of gating, so the paired statistics are reported for information
/// and the status stays `no_baseline`.
pub(crate) fn verdict(
    baseline: Option<f64>,
    s: &Summary,
    deltas: Option<&[f64]>,
    m: usize,
) -> SigVerdict {
    let alpha = bonferroni_alpha(ALPHA, m);
    let z_crit = bonferroni_z(ALPHA, m);
    let mut v = SigVerdict {
        status: "no_baseline",
        method: "none",
        scalar_fallback: false,
        alpha,
        comparisons: m,
        p_value: None,
        mean_delta: None,
        caveats: Vec::new(),
    };

    // 1. Absolute floor against the benchmark's baseline_score.
    if let Some(b) = baseline {
        if s.n < 2 {
            v.scalar_fallback = true;
            v.method = "scalar";
            v.status = if s.mean + EPS < b { "regressed" } else { "passed" };
            v.caveats.push(format!(
                "scalar fallback: n={} gives no stderr, so this is a bare mean compare, not a test",
                s.n
            ));
        } else {
            v.method = "unpaired-ci";
            let upper = s.mean + z_crit * s.stderr;
            v.status = if upper + EPS < b { "regressed" } else { "passed" };
        }
        // Bullet 3 of the honesty ledger: `baseline_score` is a scalar with no recorded stderr, so
        // this test treats it as a known constant. It is not one — it came from a run with its own
        // sampling error. The paired test below is the fix; where it can't run, this stands.
        v.caveats.push(
            "baseline_score is treated as a known constant: it carries no stderr, so this run's \
             uncertainty is accounted for and the baseline's is not"
                .to_string(),
        );
    }

    // 2. Paired per-case test against the previous comparable run.
    match deltas.and_then(paired_z) {
        Some((mean_delta, _z, p)) => {
            v.method = "paired-z";
            v.mean_delta = Some(mean_delta);
            v.p_value = Some(p);
            if baseline.is_some() && mean_delta < 0.0 && p < alpha {
                v.status = "regressed";
            }
            if baseline.is_none() {
                v.caveats.push(
                    "no baseline_score: the paired comparison is reported but does not gate"
                        .to_string(),
                );
            }
        }
        None if baseline.is_some() => v.caveats.push(
            "no comparable previous run with matching cases — fell back to the unpaired test \
             against baseline_score, which has less power"
                .to_string(),
        ),
        None => {}
    }

    if m > 1 {
        v.caveats.push(format!(
            "family-wise correction: Bonferroni over {m} targets, per-comparison α={alpha:.4} \
             (z={z_crit:.3}). Conservative by design — it trades power for not calling a false \
             regression; raise the case count to recover the power"
        ));
    }
    v
}

/// Layer the verdict's evidence onto a run report as a `significance` block. Additive JSON — old
/// runs simply lack the key — and it **names the correction method**, because a corrected verdict a
/// reader can't identify is indistinguishable from an uncorrected one.
pub(crate) fn annotate_verdict(report: &mut Value, v: &SigVerdict) {
    let correction = if v.comparisons > 1 {
        json!(format!("Bonferroni (m={}, family-wise α={ALPHA})", v.comparisons))
    } else {
        json!("none (single comparison)")
    };
    if let Some(obj) = report.as_object_mut() {
        obj.insert(
            "significance".into(),
            json!({
                "method": v.method,
                "family_wise_correction": correction,
                "alpha_per_comparison": (v.alpha * 1e6).round() / 1e6,
                "comparisons": v.comparisons,
                "p_value": v.p_value.map(|p| (p * 1e6).round() / 1e6),
                "mean_delta_vs_previous": v.mean_delta.map(|d| (d * 1000.0).round() / 1000.0),
                "caveats": v.caveats,
            }),
        );
    }
}

/// Is the gap between the top two targets real? A **paired** two-target test over the cases both
/// were scored on, at α corrected over every pair a "best" claim implicitly chose between
/// (`m·(m−1)/2` — the claim is post-hoc, so the whole family counts). Returns
/// `(mean_delta, p, significant)`; `None` when the two targets weren't scored on the same cases.
pub(crate) fn superiority(top: &[f64], runner_up: &[f64], n_targets: usize) -> Option<(f64, f64, bool)> {
    let deltas = paired_deltas(top, runner_up)?;
    let (mean_delta, _z, p) = paired_z(&deltas)?;
    let pairs = n_targets * n_targets.saturating_sub(1) / 2;
    Some((mean_delta, p, mean_delta > 0.0 && p < bonferroni_alpha(ALPHA, pairs.max(1))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn paired_deltas_refuses_mismatched_case_sets() {
        assert_eq!(paired_deltas(&[0.8, 0.9], &[0.7, 0.7]), Some(vec![0.8 - 0.7, 0.9 - 0.7]));
        assert!(paired_deltas(&[0.8, 0.9], &[0.7]).is_none(), "different n → no pairing");
        assert!(paired_deltas(&[], &[]).is_none(), "nothing to pair");
    }

    #[test]
    fn paired_z_worked_example() {
        // Deltas −0.10, −0.20, −0.30, −0.20 → mean −0.20; deviations +0.10, 0, −0.10, 0
        // → sample var = (0.01 + 0 + 0.01 + 0)/3 = 1/150 → stdev = 0.0816497
        // → stderr = 0.0816497/2 = 0.0408248 → z = −0.20/0.0408248 = −4.89898.
        let (mean, z, p) = paired_z(&[-0.10, -0.20, -0.30, -0.20]).unwrap();
        assert!(near(mean, -0.20, 1e-12));
        assert!(near(z, -4.898_979_5, 1e-6), "got z={z}");
        // Two-sided p at |z| = 4.9 is ~9.6e-7 — far below any per-comparison α we use.
        assert!(p < 1e-5, "got p={p}");
        // n < 2 has no spread to test.
        assert!(paired_z(&[0.1]).is_none());
        // A perfectly consistent shift has zero stderr: that is maximal evidence, not a discard.
        let (mean, z, p) = paired_z(&[-0.1, -0.1, -0.1]).unwrap();
        assert!(near(mean, -0.1, 1e-12) && z < -1e6 && p == 0.0, "got z={z} p={p}");
        // …and a perfectly consistent *no* change is z = 0, p = 1.
        let (_, z, p) = paired_z(&[0.0, 0.0, 0.0]).unwrap();
        assert!(z == 0.0 && near(p, 1.0, 1e-6));
    }

    #[test]
    fn paired_test_catches_a_drop_the_unpaired_ci_misses() {
        // Hard-and-easy cases: scores span 0.2..0.95, so the unpaired CI on the mean is wide.
        let base = [0.95, 0.90, 0.55, 0.25, 0.85, 0.60, 0.30, 0.90];
        let run: Vec<f64> = base.iter().map(|b| b - 0.08).collect(); // every case dropped 0.08
        let s = Summary::of(&run);
        // Unpaired alone: the CI on the mean straddles the baseline → "passed" (no evidence).
        let unpaired = verdict(Some(0.6625), &s, None, 1);
        assert_eq!(unpaired.status, "passed", "between-case spread swamps a real 0.08 drop");
        // Paired: every delta is exactly −0.08 → overwhelming evidence → regressed.
        let deltas = paired_deltas(&run, &base).unwrap();
        let paired = verdict(Some(0.6625), &s, Some(&deltas), 1);
        assert_eq!(paired.status, "regressed");
        assert_eq!(paired.method, "paired-z");
        assert!(near(paired.mean_delta.unwrap(), -0.08, 1e-9));
        assert_eq!(paired.p_value, Some(0.0));
    }

    #[test]
    fn paired_improvement_never_reads_as_a_regression() {
        let base = [0.5, 0.6, 0.4, 0.7];
        let run = [0.6, 0.7, 0.5, 0.8];
        let s = Summary::of(&run);
        let deltas = paired_deltas(&run, &base).unwrap();
        let v = verdict(Some(0.5), &s, Some(&deltas), 1);
        assert_eq!(v.status, "passed");
        assert!(v.mean_delta.unwrap() > 0.0);
    }

    #[test]
    fn family_wise_correction_raises_the_bar_across_targets() {
        // A run whose mean is 2.2 stderrs below baseline: significant alone (z_crit 1.96), NOT
        // significant once corrected across 6 targets (z_crit 2.638). mean 0.5, stderr 0.05,
        // baseline 0.61 → upper(m=1) = 0.5 + 1.96·0.05 = 0.598 < 0.61 → regressed.
        //                → upper(m=6) = 0.5 + 2.638·0.05 = 0.6319 > 0.61 → passed.
        let s = Summary { n: 25, mean: 0.5, stdev: 0.25, stderr: 0.05 };
        assert_eq!(verdict(Some(0.61), &s, None, 1).status, "regressed");
        let corrected = verdict(Some(0.61), &s, None, 6);
        assert_eq!(corrected.status, "passed", "one of six targets needs stronger evidence");
        assert!(near(corrected.alpha, 0.05 / 6.0, 1e-12));
        assert!(
            corrected.caveats.iter().any(|c| c.contains("Bonferroni")),
            "the correction must be disclosed, not silently applied"
        );
        // A genuinely large regression still trips at m = 6 — the gate is not disarmed.
        let bad = Summary { n: 25, mean: 0.5, stdev: 0.25, stderr: 0.05 };
        assert_eq!(verdict(Some(0.80), &bad, None, 6).status, "regressed");
    }

    #[test]
    fn baseline_uncertainty_is_always_disclosed() {
        let s = Summary::of(&[0.7, 0.8, 0.75]);
        let v = verdict(Some(0.6), &s, None, 1);
        assert!(v.caveats.iter().any(|c| c.contains("known constant")));
        assert!(
            v.caveats.iter().any(|c| c.contains("no comparable previous run")),
            "the unpaired fallback must be flagged as such"
        );
        // No baseline at all → nothing claimed, no caveats about a comparison that didn't happen.
        let none = verdict(None, &s, None, 1);
        assert_eq!(none.status, "no_baseline");
        assert_eq!(none.method, "none");
        assert!(none.caveats.is_empty());
    }

    #[test]
    fn paired_stats_report_but_do_not_gate_without_a_baseline() {
        let base = [0.9, 0.9, 0.9, 0.9];
        let run = [0.1, 0.1, 0.1, 0.1];
        let deltas = paired_deltas(&run, &base).unwrap();
        let v = verdict(None, &Summary::of(&run), Some(&deltas), 1);
        assert_eq!(v.status, "no_baseline", "a benchmark with no baseline opted out of gating");
        assert!(near(v.mean_delta.unwrap(), -0.8, 1e-9), "…but the drop is still reported");
        assert!(v.caveats.iter().any(|c| c.contains("does not gate")));
    }

    #[test]
    fn superiority_needs_real_separation() {
        // Two targets 0.01 apart with the gap flapping in sign: no significant winner.
        let a = [0.80, 0.70, 0.90, 0.60, 0.85];
        let b = [0.79, 0.72, 0.88, 0.61, 0.84];
        let (delta, p, significant) = superiority(&a, &b, 2).unwrap();
        assert!(delta > 0.0 && delta < 0.02);
        assert!(!significant, "a 0.01 gap with overlapping noise is not a winner (p={p})");
        // A consistent 0.15 lead on every case IS separation.
        let a2: Vec<f64> = b.iter().map(|x| x + 0.15).collect();
        let (delta, _p, significant) = superiority(&a2, &b, 2).unwrap();
        assert!(near(delta, 0.15, 1e-9) && significant);
        // Six targets ⇒ 15 implicit pairwise choices ⇒ a stricter bar, still cleared here.
        assert!(superiority(&a2, &b, 6).unwrap().2);
        // Mismatched case sets → no claim at all.
        assert!(superiority(&a2, &b[..3], 2).is_none());
    }
}
