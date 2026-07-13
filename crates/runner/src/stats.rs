//! Pure statistics for significance-aware benchmark verdicts: a sample summary (mean ± stderr, n), a
//! confidence-interval regression test against a baseline, and a stdev-based stability measure. No
//! I/O — every function is a deterministic transform, unit-tested beside it, so the runner's
//! quality gate rests on math that can't silently drift.

use serde_json::{json, Value};

/// z for a ~95% two-sided normal confidence interval.
pub(crate) const Z_95: f64 = 1.959_963_984_540_054;

/// The float slack that keeps pure float noise from tripping a comparison (mirrors `util::run_status`).
const EPS: f64 = 1e-9;

/// Below this many samples the nearest-rank p95 collapses toward the max — annotated as a caveat.
const SMALL_N: usize = 20;

/// Sample summary of a set of per-case scores.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Summary {
    pub(crate) n: usize,
    pub(crate) mean: f64,
    /// Sample standard deviation (Bessel's correction); 0 when `n < 2`.
    pub(crate) stdev: f64,
    /// Standard error of the mean = stdev / √n; 0 when `n < 2`.
    pub(crate) stderr: f64,
}

impl Summary {
    /// Summarize a slice of scores. Empty → all-zero; `n == 1` → mean only (no spread).
    pub(crate) fn of(xs: &[f64]) -> Summary {
        let n = xs.len();
        if n == 0 {
            return Summary { n: 0, mean: 0.0, stdev: 0.0, stderr: 0.0 };
        }
        let mean = xs.iter().sum::<f64>() / n as f64;
        if n < 2 {
            return Summary { n, mean, stdev: 0.0, stderr: 0.0 };
        }
        let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
        let stdev = var.sqrt();
        Summary { n, mean, stdev, stderr: stdev / (n as f64).sqrt() }
    }

    /// Lower/upper bound of the ~95% CI on the mean (mean ± 1.96·stderr).
    pub(crate) fn ci95(&self) -> (f64, f64) {
        let h = Z_95 * self.stderr;
        (self.mean - h, self.mean + h)
    }
}

/// A significance-aware run verdict against a baseline, returning `(status, scalar_fallback)`.
///
/// `regressed` requires the whole ~95% CI on the mean to sit *below* the baseline — so a noisy
/// 3-case run can't trip it as easily as a 3000-case run. When `n < 2` there is no stderr, so we
/// fall back to the plain scalar compare the simple/rubric modes have always used (with the same
/// 1e-9 slack) and report `scalar_fallback = true` so the run can annotate itself.
pub(crate) fn significance_verdict(baseline: Option<f64>, s: &Summary) -> (&'static str, bool) {
    let Some(b) = baseline else {
        return ("no_baseline", false);
    };
    if s.n < 2 {
        let status = if s.mean + EPS < b { "regressed" } else { "passed" };
        return (status, true);
    }
    let (_, upper) = s.ci95();
    if upper + EPS < b {
        ("regressed", false)
    } else {
        ("passed", false)
    }
}

/// Stability of a set of scores as a 0..1 agreement: `1 − min(1, 2·σ)`, where σ is the *population*
/// standard deviation. 1.0 = identical; it degrades smoothly with spread, so — unlike the old
/// `1 − (max − min)` — a single outlier no longer dominates the measure. Fewer than two samples →
/// 1.0 (nothing to disagree on).
pub(crate) fn stability(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 1.0;
    }
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n; // population σ
    (1.0 - 2.0 * var.sqrt()).clamp(0.0, 1.0)
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Layer the significance block (mean ± stderr, n, the ~95% CI, `stdev`) and any caveats onto a run
/// report object. Additive JSON only — old runs simply lack these keys. Caveats collected:
/// scalar-fallback (n<2) and the small-n percentile caveat (n<20).
pub(crate) fn annotate_significance(report: &mut Value, s: &Summary, scalar_fallback: bool) {
    let (lo, hi) = s.ci95();
    let mut caveats: Vec<String> = Vec::new();
    if scalar_fallback {
        caveats.push(format!("scalar fallback, n={}", s.n));
    }
    if s.n > 0 && s.n < SMALL_N {
        caveats.push(format!("small-n: p95 ≈ max (n={} < {SMALL_N})", s.n));
    }
    if let Some(obj) = report.as_object_mut() {
        obj.insert("n".into(), json!(s.n));
        obj.insert("mean".into(), json!(round3(s.mean)));
        obj.insert("stderr".into(), json!(round3(s.stderr)));
        obj.insert("stdev".into(), json!(round3(s.stdev)));
        obj.insert("ci95".into(), json!([round3(lo), round3(hi)]));
        if !caveats.is_empty() {
            obj.insert("caveats".into(), json!(caveats));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn summary_edge_cases() {
        let s0 = Summary::of(&[]);
        assert_eq!(s0.n, 0);
        assert!(approx(s0.mean, 0.0) && approx(s0.stderr, 0.0));
        let s1 = Summary::of(&[0.7]);
        assert_eq!(s1.n, 1);
        assert!(approx(s1.mean, 0.7) && approx(s1.stderr, 0.0) && approx(s1.stdev, 0.0));
        // n=2 known vector: mean 0.5, sample stdev sqrt(0.5·... )
        let s2 = Summary::of(&[0.0, 1.0]);
        assert_eq!(s2.n, 2);
        assert!(approx(s2.mean, 0.5));
        assert!(approx(s2.stdev, (0.5f64).sqrt())); // var = (0.25+0.25)/1 = 0.5
        assert!(approx(s2.stderr, (0.5f64).sqrt() / 2.0_f64.sqrt()));
    }

    #[test]
    fn summary_known_vector() {
        // 2,4,4,4,5,5,7,9 → mean 5, sample stdev 2.138... (classic example, n-1 denominator).
        let s = Summary::of(&[2., 4., 4., 4., 5., 5., 7., 9.]);
        assert!(approx(s.mean, 5.0));
        assert!(approx(s.stdev, (32.0f64 / 7.0).sqrt()));
    }

    #[test]
    fn no_baseline_never_regresses() {
        let (status, fallback) = significance_verdict(None, &Summary::of(&[0.1, 0.2]));
        assert_eq!(status, "no_baseline");
        assert!(!fallback);
    }

    #[test]
    fn small_n_uses_scalar_fallback() {
        // n=1 below baseline → regressed, flagged as a scalar fallback.
        let (status, fallback) = significance_verdict(Some(0.8), &Summary::of(&[0.5]));
        assert_eq!(status, "regressed");
        assert!(fallback);
        // n=1 at/above baseline → passed, still a scalar fallback.
        let (status, fallback) = significance_verdict(Some(0.4), &Summary::of(&[0.5]));
        assert_eq!(status, "passed");
        assert!(fallback);
    }

    #[test]
    fn ci_excludes_baseline_regresses() {
        // Tight cluster well below baseline → whole CI < baseline → regressed (no fallback).
        let s = Summary::of(&[0.50, 0.52, 0.48, 0.50, 0.51]);
        let (status, fallback) = significance_verdict(Some(0.8), &s);
        assert_eq!(status, "regressed");
        assert!(!fallback);
    }

    #[test]
    fn wide_ci_overlapping_baseline_holds() {
        // n=2 wild spread: CI is enormous and straddles the baseline → not enough evidence → passed.
        let s = Summary::of(&[0.0, 1.0]);
        let (upper_below, _) = (s.ci95().1 + EPS < 0.9, ());
        assert!(!upper_below);
        let (status, fallback) = significance_verdict(Some(0.9), &s);
        assert_eq!(status, "passed");
        assert!(!fallback);
    }

    #[test]
    fn stability_stdev_based() {
        // Identical → 1.0; <2 samples → 1.0.
        assert!(approx(stability(&[0.8, 0.8, 0.8]), 1.0));
        assert!(approx(stability(&[]), 1.0));
        assert!(approx(stability(&[0.4]), 1.0));
        // Max spread over [0,1] → σ=0.5 → 1 − 2·0.5 = 0.0 (clamped, never negative).
        assert!(approx(stability(&[0.0, 1.0]), 0.0));
        assert!(approx(stability(&[-0.5, 1.0]), 0.0));
        // Known moderate spread: 0.6,0.9,0.7 → pop σ ≈ 0.12472 → 1 − 0.24944 ≈ 0.75056.
        assert!(approx(stability(&[0.6, 0.9, 0.7]), 1.0 - 2.0 * (14.0f64 / 900.0).sqrt()));
    }

    #[test]
    fn annotate_adds_block_and_caveats() {
        let mut r = json!({ "mode": "simple" });
        annotate_significance(&mut r, &Summary::of(&[0.5]), true);
        assert_eq!(r["n"], json!(1));
        assert_eq!(r["mean"], json!(0.5));
        // n=1 → both a scalar-fallback caveat and the small-n caveat.
        let caveats = r["caveats"].as_array().unwrap();
        assert_eq!(caveats.len(), 2);
        assert!(caveats[0].as_str().unwrap().contains("scalar fallback"));

        // A large clean sample → no caveats key at all.
        let big: Vec<f64> = (0..30).map(|_| 0.7).collect();
        let mut r2 = json!({});
        annotate_significance(&mut r2, &Summary::of(&big), false);
        assert!(r2.get("caveats").is_none());
        assert_eq!(r2["n"], json!(30));
    }
}
