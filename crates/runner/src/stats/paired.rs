//! The verdict math behind "B beats A": a **paired** per-case test against the previous run, the
//! absolute-floor test against the benchmark's `baseline_score`, and the family-wise correction that
//! keeps a six-target comparison from tripping a false `regressed` a quarter of the time.
//!
//! Why paired: the same cases are judged in both runs, so the per-case *difference* removes
//! between-case variance entirely. An unpaired comparison of two means over hard-and-easy cases is
//! dominated by how hard the cases are; the paired one measures only what changed. That is
//! typically several times more power at the same sample size, which is the difference between
//! "we cannot tell" and a usable gate.
//!
//! "The same cases" is a claim about **identity**, and it is checked as one — see [`super::cases`].
//! It used to be checked as a count, which is how two targets that errored on different cases came
//! to be differenced against each other while the paired stderr still reported that between-case
//! variance had been removed.

use serde_json::{json, Value};

use super::cases::{paired_deltas_by_case, CaseScore, PairedByCase, Unpairable};
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
    /// Cases the paired test actually ran over — the intersection, not this run's case count. The
    /// figure that belongs in a power disclosure; `summary.n` is a different number.
    pub(crate) paired_cases: Option<usize>,
    /// Cases judged by only one of the two runs, and so left out of the paired test.
    pub(crate) paired_dropped: Option<usize>,
    /// Honest limitations of this particular verdict, in plain words.
    pub(crate) caveats: Vec<String>,
}

/// Per-case differences `run[i] − baseline[i]`, paired **by position**.
///
/// This checks the two vectors are the same non-zero **length**, and nothing else. Equal length is
/// not a matching case set: a compare run's per-case vector is compacted past errored cells, so two
/// targets that each failed a different case arrive here the same length and one position out — and
/// differencing two different cases *adds* between-case variance while the paired stderr still says
/// it was removed. This function cannot detect that, so it is correct **only** for callers whose two
/// vectors are built from one collection and are therefore aligned by construction (the reference
/// case is `calibrate_batch`, which maps `single` and `batched` out of a single `pairs` vector).
///
/// Anything pairing two independently-collected vectors must use
/// [`super::cases::paired_deltas_by_case`], which aligns on the case id.
pub(crate) fn paired_deltas(run: &[f64], baseline: &[f64]) -> Option<Vec<f64>> {
    if run.is_empty() || run.len() != baseline.len() {
        return None;
    }
    Some(run.iter().zip(baseline).map(|(a, b)| a - b).collect())
}

/// What the by-case pairing produced, handed to [`verdict`] so the reduced n and the subset are
/// disclosed *inside* the function that builds the caveats rather than by whoever remembers to.
pub(crate) struct PairedEvidence<'a> {
    pub(crate) pairing: &'a PairedByCase,
    /// The baseline run's `cases` array was a bounded preview (`attach_cases` keeps the FIRST
    /// `MAX_LOGGED_CASES`), so the pairing can only ever have covered that prefix.
    pub(crate) preview_limited: bool,
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
/// this run's per-case scores; `ev` is the by-case pairing against the previous comparable run, when
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
    ev: Option<&PairedEvidence>,
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
        paired_cases: None,
        paired_dropped: None,
        caveats: Vec::new(),
    };

    // 1. Absolute floor against the benchmark's baseline_score.
    if let Some(b) = baseline {
        if s.n < 2 {
            v.scalar_fallback = true;
            v.method = "scalar";
            v.status = if s.mean + EPS < b {
                "regressed"
            } else {
                "passed"
            };
            v.caveats.push(format!(
                "scalar fallback: n={} gives no stderr, so this is a bare mean compare, not a test",
                s.n
            ));
        } else {
            v.method = "unpaired-ci";
            let upper = s.mean + z_crit * s.stderr;
            v.status = if upper + EPS < b {
                "regressed"
            } else {
                "passed"
            };
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

    // 2. Paired per-case test against the previous comparable run, over the cases BOTH runs judged.
    match ev.and_then(|e| paired_z(&e.pairing.deltas).map(|z| (e, z))) {
        Some((e, (mean_delta, _z, p))) => {
            v.method = "paired-z";
            v.mean_delta = Some(mean_delta);
            v.p_value = Some(p);
            v.paired_cases = Some(e.pairing.retained);
            v.paired_dropped = Some(e.pairing.dropped);
            if baseline.is_some() && mean_delta < 0.0 && p < alpha {
                v.status = "regressed";
            }
            // An intersection is a valid paired test over fewer cases, so it is reported — with the
            // count it actually ran over, because `n` elsewhere in the report is this run's case
            // count and a reader who takes it for the paired n overstates the power by that gap.
            if e.pairing.is_subset() {
                v.caveats.push(e.pairing.subset_caveat());
            }
            if e.preview_limited {
                v.caveats.push(
                    "the previous run's report logged only a bounded preview of its cases (the \
                     FIRST of them), so this pairing is over that prefix — the same cases in both \
                     runs, and therefore a valid paired test, but a systematic subset rather than a \
                     random one"
                        .to_string(),
                );
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
        json!(format!(
            "Bonferroni (m={}, family-wise α={ALPHA})",
            v.comparisons
        ))
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
                // The paired test's own n and what it left out. `n` elsewhere on the report is this
                // run's case count, which is the same number only when both runs judged the same
                // cases — the assumption this whole block exists to stop being made silently.
                "paired_cases": v.paired_cases,
                "paired_cases_dropped": v.paired_dropped,
                "caveats": v.caveats,
            }),
        );
    }
}

/// A tested "B beats A", with the sample it rests on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Superiority {
    pub(crate) mean_delta: f64,
    pub(crate) p: f64,
    pub(crate) significant: bool,
    /// Cases both targets judged — the n of THIS test, and the only n that belongs beside its p.
    pub(crate) retained: usize,
    /// Cases judged by exactly one of the two, and therefore not in the test.
    pub(crate) dropped: usize,
}

/// Is the gap between the top two targets real? A **paired** two-target test over the cases both
/// were scored on — matched by case id, so "both" means both and not "the same number of them" — at
/// α corrected over every pair a "best" claim implicitly chose between (`m·(m−1)/2`; the claim is
/// post-hoc, so the whole family counts).
///
/// `Err` carries *which* refusal it was, because "they share no cases", "one of them scored nothing"
/// and "they share exactly one case" are three different things to tell an operator.
pub(crate) fn superiority(
    top: &[CaseScore],
    runner_up: &[CaseScore],
    n_targets: usize,
) -> Result<Superiority, Unpairable> {
    let pairing = paired_deltas_by_case(top, runner_up)?;
    let (mean_delta, _z, p) =
        paired_z(&pairing.deltas).ok_or(Unpairable::TooFewShared(pairing.retained))?;
    let pairs = n_targets * n_targets.saturating_sub(1) / 2;
    Ok(Superiority {
        mean_delta,
        p,
        significant: mean_delta > 0.0 && p < bonferroni_alpha(ALPHA, pairs.max(1)),
        retained: pairing.retained,
        dropped: pairing.dropped,
    })
}

#[cfg(test)]
mod tests {
    use super::super::cases::values;
    use super::*;

    fn near(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() < tol
    }

    fn ev(p: &PairedByCase) -> PairedEvidence<'_> {
        PairedEvidence {
            pairing: p,
            preview_limited: false,
        }
    }

    fn cs(pairs: &[(u32, f64)]) -> Vec<CaseScore> {
        pairs.iter().map(|&(c, s)| CaseScore::new(c, s)).collect()
    }

    /// Scores over cases 1..n, the shape a run with no errored cell produces.
    fn seq(scores: &[f64]) -> Vec<CaseScore> {
        scores
            .iter()
            .enumerate()
            .map(|(i, s)| CaseScore::new(i as u32 + 1, *s))
            .collect()
    }

    fn pair(run: &[f64], base: &[f64]) -> PairedByCase {
        paired_deltas_by_case(&seq(run), &seq(base)).expect("same case set")
    }

    /// The position-pairing form checks **length only**, and this pins that it does — including the
    /// hazard, so nobody reads the guard as more than it is. (Was
    /// `paired_deltas_refuses_mismatched_case_sets`, a name that claimed the check this function has
    /// never performed. Renamed and extended: it now also pins that two vectors from DIFFERENT case
    /// sets pair silently here, which is why every caller that collects its two vectors
    /// independently must use `paired_deltas_by_case` instead.)
    #[test]
    fn paired_deltas_pairs_by_position_and_checks_length_only() {
        assert_eq!(
            paired_deltas(&[0.8, 0.9], &[0.7, 0.7]),
            Some(vec![0.8 - 0.7, 0.9 - 0.7])
        );
        assert!(
            paired_deltas(&[0.8, 0.9], &[0.7]).is_none(),
            "different n → no pairing"
        );
        assert!(paired_deltas(&[], &[]).is_none(), "nothing to pair");
        // The hazard, stated as a test: these are cases 2..4 and cases 1..3 of one ladder, so every
        // real per-case difference is zero — and position pairing reports a flat +0.1 anyway.
        let offset = paired_deltas(&[0.2, 0.3, 0.4], &[0.1, 0.2, 0.3]).expect("same length");
        assert!(
            offset.iter().all(|d| near(*d, 0.1, 1e-12)),
            "length is not identity — the position form reports a gap that is purely the offset, \
             which is exactly what the by-case form exists to prevent: {offset:?}"
        );
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
        assert!(
            near(mean, -0.1, 1e-12) && z < -1e6 && p == 0.0,
            "got z={z} p={p}"
        );
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
        assert_eq!(
            unpaired.status, "passed",
            "between-case spread swamps a real 0.08 drop"
        );
        // Paired: every delta is exactly −0.08 → overwhelming evidence → regressed.
        let p = pair(&run, &base);
        let paired = verdict(Some(0.6625), &s, Some(&ev(&p)), 1);
        assert_eq!(paired.status, "regressed");
        assert_eq!(paired.method, "paired-z");
        assert!(near(paired.mean_delta.unwrap(), -0.08, 1e-9));
        assert_eq!(paired.p_value, Some(0.0));
        assert_eq!(paired.paired_cases, Some(8), "the test's own n is reported");
        assert_eq!(paired.paired_dropped, Some(0));
        assert!(
            !paired.caveats.iter().any(|c| c.contains("judged by both")),
            "nothing was dropped, so no subset caveat: {:?}",
            paired.caveats
        );
    }

    #[test]
    fn paired_improvement_never_reads_as_a_regression() {
        let base = [0.5, 0.6, 0.4, 0.7];
        let run = [0.6, 0.7, 0.5, 0.8];
        let s = Summary::of(&run);
        let p = pair(&run, &base);
        let v = verdict(Some(0.5), &s, Some(&ev(&p)), 1);
        assert_eq!(v.status, "passed");
        assert!(v.mean_delta.unwrap() > 0.0);
    }

    #[test]
    fn family_wise_correction_raises_the_bar_across_targets() {
        // A run whose mean is 2.2 stderrs below baseline: significant alone (z_crit 1.96), NOT
        // significant once corrected across 6 targets (z_crit 2.638). mean 0.5, stderr 0.05,
        // baseline 0.61 → upper(m=1) = 0.5 + 1.96·0.05 = 0.598 < 0.61 → regressed.
        //                → upper(m=6) = 0.5 + 2.638·0.05 = 0.6319 > 0.61 → passed.
        let s = Summary {
            n: 25,
            mean: 0.5,
            stdev: 0.25,
            stderr: 0.05,
        };
        assert_eq!(verdict(Some(0.61), &s, None, 1).status, "regressed");
        let corrected = verdict(Some(0.61), &s, None, 6);
        assert_eq!(
            corrected.status, "passed",
            "one of six targets needs stronger evidence"
        );
        assert!(near(corrected.alpha, 0.05 / 6.0, 1e-12));
        assert!(
            corrected.caveats.iter().any(|c| c.contains("Bonferroni")),
            "the correction must be disclosed, not silently applied"
        );
        // A genuinely large regression still trips at m = 6 — the gate is not disarmed.
        let bad = Summary {
            n: 25,
            mean: 0.5,
            stdev: 0.25,
            stderr: 0.05,
        };
        assert_eq!(verdict(Some(0.80), &bad, None, 6).status, "regressed");
    }

    #[test]
    fn baseline_uncertainty_is_always_disclosed() {
        let s = Summary::of(&[0.7, 0.8, 0.75]);
        let v = verdict(Some(0.6), &s, None, 1);
        assert!(v.caveats.iter().any(|c| c.contains("known constant")));
        assert!(
            v.caveats
                .iter()
                .any(|c| c.contains("no comparable previous run")),
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
        let p = pair(&run, &base);
        let v = verdict(None, &Summary::of(&run), Some(&ev(&p)), 1);
        assert_eq!(
            v.status, "no_baseline",
            "a benchmark with no baseline opted out of gating"
        );
        assert!(
            near(v.mean_delta.unwrap(), -0.8, 1e-9),
            "…but the drop is still reported"
        );
        assert!(v.caveats.iter().any(|c| c.contains("does not gate")));
    }

    #[test]
    fn superiority_needs_real_separation() {
        // Two targets 0.01 apart with the gap flapping in sign: no significant winner.
        let a = [0.80, 0.70, 0.90, 0.60, 0.85];
        let b = [0.79, 0.72, 0.88, 0.61, 0.84];
        let r = superiority(&seq(&a), &seq(&b), 2).expect("same cases");
        assert!(r.mean_delta > 0.0 && r.mean_delta < 0.02);
        assert!(
            !r.significant,
            "a 0.01 gap with overlapping noise is not a winner (p={})",
            r.p
        );
        assert_eq!((r.retained, r.dropped), (5, 0));
        // A consistent 0.15 lead on every case IS separation.
        let a2: Vec<f64> = b.iter().map(|x| x + 0.15).collect();
        let r = superiority(&seq(&a2), &seq(&b), 2).expect("same cases");
        assert!(near(r.mean_delta, 0.15, 1e-9) && r.significant);
        // Six targets ⇒ 15 implicit pairwise choices ⇒ a stricter bar, still cleared here.
        assert!(
            superiority(&seq(&a2), &seq(&b), 6)
                .expect("same cases")
                .significant
        );
        // **Disjoint** case sets → no claim at all, and the refusal says which one it is. (This
        // assertion used to read `superiority(&a2, &b[..3], 2).is_none()` — a shorter *vector*,
        // which under by-case pairing is cases 1..3 of the same ladder and pairs perfectly well on
        // the overlap. It now pins the thing it always claimed to: a difference of case IDENTITY.)
        let elsewhere = cs(&[(90, 0.5), (91, 0.6), (92, 0.7)]);
        assert_eq!(
            superiority(&seq(&a2), &elsewhere, 2).unwrap_err(),
            Unpairable::Disjoint
        );
        // …and the case the old assertion actually exercised: a shorter vector over a PREFIX of the
        // same cases is a valid test over the intersection, with the drop counted.
        let short = superiority(&seq(&a2), &seq(&b[..3]), 2).expect("cases 1..3 overlap");
        assert_eq!((short.retained, short.dropped), (3, 2));
        assert!(near(short.mean_delta, 0.15, 1e-9));
    }

    /// **The headline, at the level of the test that decides `best`.** Two targets that each errored
    /// on a different case: equal counts, offset case sets. Positional pairing sees a flat +0.15 on
    /// every case — zero stderr, p = 0, a *certain* win — between two targets that scored
    /// identically on every case they both judged.
    #[test]
    fn superiority_never_pairs_two_different_cases() {
        let d = [0.10, 0.25, 0.40, 0.55, 0.70, 0.85]; // a difficulty ladder
        let a = cs(&[(2, d[1]), (3, d[2]), (4, d[3]), (5, d[4]), (6, d[5])]); // errored case 1
        let b = cs(&[(1, d[0]), (2, d[1]), (3, d[2]), (4, d[3]), (5, d[4])]); // errored case 6
        assert_eq!(a.len(), b.len(), "the old count check would have passed");
        let r = superiority(&a, &b, 2).expect("cases 2..5 overlap");
        assert!(
            !r.significant && near(r.mean_delta, 0.0, 1e-12),
            "identical on every shared case — there is nothing to separate: {r:?}"
        );
        assert_eq!(
            (r.retained, r.dropped),
            (4, 2),
            "the reduced n and the drop both travel with the claim"
        );
    }

    /// Sharing exactly one case is not the same refusal as sharing none, and neither is silence.
    #[test]
    fn superiority_says_which_refusal_it_is() {
        let a = cs(&[(1, 0.9), (2, 0.9), (3, 0.9)]);
        assert_eq!(
            superiority(&a, &cs(&[(3, 0.1), (7, 0.1)]), 2).unwrap_err(),
            Unpairable::TooFewShared(1)
        );
        assert_eq!(superiority(&a, &[], 2).unwrap_err(), Unpairable::Empty);
        assert_eq!(
            superiority(&a, &cs(&[(1, 0.1), (1, 0.2), (2, 0.3)]), 2).unwrap_err(),
            Unpairable::DuplicateCaseIds
        );
    }

    /// A subset pairing and a preview-limited baseline are both disclosed on the verdict, with the
    /// paired test's own n — never this run's case count, which is the larger, flattering number.
    #[test]
    fn a_subset_pairing_is_disclosed_with_its_real_n() {
        // This run judged cases 1..6; the baseline only recorded 1..4.
        let run = seq(&[0.5, 0.5, 0.5, 0.5, 0.5, 0.5]);
        let base = seq(&[0.7, 0.7, 0.7, 0.7]);
        let p = paired_deltas_by_case(&run, &base).expect("cases 1..4 overlap");
        let s = Summary::of(&values(&run));
        assert_eq!(s.n, 6, "the run's own n is six");
        let v = verdict(
            Some(0.4),
            &s,
            Some(&PairedEvidence {
                pairing: &p,
                preview_limited: true,
            }),
            1,
        );
        assert_eq!(v.paired_cases, Some(4), "the test ran over four, not six");
        assert_eq!(v.paired_dropped, Some(2));
        assert!(
            v.caveats.iter().any(|c| c.contains("judged by both")),
            "a subset pairing must say so: {:?}",
            v.caveats
        );
        assert!(
            v.caveats.iter().any(|c| c.contains("bounded preview")),
            "a preview-limited baseline must say so: {:?}",
            v.caveats
        );
        // …and both facts reach the report, not just the in-memory verdict.
        let mut report = json!({ "mode": "compare" });
        annotate_verdict(&mut report, &v);
        assert_eq!(report["significance"]["paired_cases"], json!(4));
        assert_eq!(report["significance"]["paired_cases_dropped"], json!(2));
    }
}
