//! Judge↔human calibration: agreement metrics that say whether an LLM-as-judge rubric can be
//! *trusted*. Pure and I/O-free — the runner's `calibrate` command feeds it (human, judge) score
//! pairs from a human-labeled set. See docs/BENCHMARK_FRAMEWORK.md §3 (Calibration & reliability).

use serde::{Deserialize, Serialize};

/// One human-labeled calibration case (read from a JSONL/JSON file). `human_score` is the
/// ground-truth overall quality in 0..1 that the judge is measured against; `output` is the
/// already-produced response being judged (calibration is judge-only — it does not generate).
#[derive(Debug, Clone, Deserialize)]
pub struct CalibrationItem {
    pub input: String,
    pub output: String,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub expected: Option<String>,
    pub human_score: f64,
    /// Optional short label for the case (shown in the per-item table).
    #[serde(default)]
    pub note: Option<String>,
}

/// Judge↔human agreement computed over a set of paired scores.
#[derive(Debug, Clone, Serialize)]
pub struct Agreement {
    pub n: usize,
    /// Pearson correlation of judge vs human scores. 0 if undefined (n<2 or no variance).
    pub pearson: f64,
    /// Mean absolute error, mean(|judge - human|).
    pub mae: f64,
    /// Root-mean-square error.
    pub rmse: f64,
    /// mean(judge) - mean(human): >0 ⇒ the judge is more generous than the human.
    pub bias: f64,
    /// Cohen's κ on the binarized pass/fail decision at `threshold`.
    pub cohen_kappa: f64,
    /// Fraction of items where judge and human agree on pass/fail.
    pub agreement_rate: f64,
    pub human_pass_rate: f64,
    pub judge_pass_rate: f64,
    pub threshold: f64,
    pub kappa_bar: f64,
    /// κ ≥ `kappa_bar` ⇒ the rubric/judge is considered trustworthy.
    pub trusted: bool,
}

/// Compute agreement from `(human, judge)` pairs (both in 0..1), binarizing pass/fail at `threshold`
/// and flagging "trusted" when Cohen's κ clears `kappa_bar`.
pub fn agreement(pairs: &[(f64, f64)], threshold: f64, kappa_bar: f64) -> Agreement {
    let n = pairs.len();
    if n == 0 {
        return Agreement {
            n: 0,
            pearson: 0.0,
            mae: 0.0,
            rmse: 0.0,
            bias: 0.0,
            cohen_kappa: 0.0,
            agreement_rate: 0.0,
            human_pass_rate: 0.0,
            judge_pass_rate: 0.0,
            threshold,
            kappa_bar,
            trusted: false,
        };
    }
    let nf = n as f64;
    let pass = |s: f64| s >= threshold;

    let (mut abs_sum, mut sq_sum, mut hsum, mut jsum) = (0.0, 0.0, 0.0, 0.0);
    let (mut hp, mut jp, mut both_pass, mut both_fail) = (0u32, 0u32, 0u32, 0u32);
    for &(h, j) in pairs {
        abs_sum += (j - h).abs();
        sq_sum += (j - h).powi(2);
        hsum += h;
        jsum += j;
        let (ph, pj) = (pass(h), pass(j));
        hp += ph as u32;
        jp += pj as u32;
        both_pass += (ph && pj) as u32;
        both_fail += (!ph && !pj) as u32;
    }

    let mae = abs_sum / nf;
    let rmse = (sq_sum / nf).sqrt();
    let (hmean, jmean) = (hsum / nf, jsum / nf);
    let bias = jmean - hmean;

    // Pearson correlation.
    let (mut cov, mut vh, mut vj) = (0.0, 0.0, 0.0);
    for &(h, j) in pairs {
        cov += (h - hmean) * (j - jmean);
        vh += (h - hmean).powi(2);
        vj += (j - jmean).powi(2);
    }
    let pearson = if vh > 0.0 && vj > 0.0 {
        cov / (vh.sqrt() * vj.sqrt())
    } else {
        0.0
    };

    // Cohen's κ on the pass/fail labels.
    let po = (both_pass + both_fail) as f64 / nf;
    let (hpr, jpr) = (hp as f64 / nf, jp as f64 / nf);
    let pe = hpr * jpr + (1.0 - hpr) * (1.0 - jpr);
    let cohen_kappa = if (1.0 - pe).abs() < 1e-12 {
        // Both raters put everything in one class: defined as perfect iff they fully agree.
        if (po - 1.0).abs() < 1e-12 {
            1.0
        } else {
            0.0
        }
    } else {
        (po - pe) / (1.0 - pe)
    };

    Agreement {
        n,
        pearson,
        mae,
        rmse,
        bias,
        cohen_kappa,
        agreement_rate: po,
        human_pass_rate: hpr,
        judge_pass_rate: jpr,
        threshold,
        kappa_bar,
        trusted: cohen_kappa >= kappa_bar,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn perfect_agreement() {
        // Mixed pass/fail, judge mirrors human exactly.
        let a = agreement(&[(0.9, 0.9), (0.2, 0.2), (0.8, 0.8)], 0.7, 0.6);
        assert!(approx(a.cohen_kappa, 1.0));
        assert!(approx(a.pearson, 1.0));
        assert!(approx(a.mae, 0.0));
        assert!(approx(a.agreement_rate, 1.0));
        assert!(a.trusted);
    }

    #[test]
    fn total_disagreement() {
        // Judge inverts the human's pass/fail every time.
        let a = agreement(&[(0.9, 0.2), (0.2, 0.9)], 0.7, 0.6);
        assert!(approx(a.cohen_kappa, -1.0));
        assert!(approx(a.pearson, -1.0));
        assert!(approx(a.agreement_rate, 0.0));
        assert!(!a.trusted);
    }

    #[test]
    fn bias_and_mae() {
        // Judge is consistently 0.2 more generous than the human.
        let a = agreement(&[(0.5, 0.7), (0.6, 0.8), (0.1, 0.3)], 0.7, 0.6);
        assert!(approx(a.bias, 0.2));
        assert!(approx(a.mae, 0.2));
    }

    #[test]
    fn empty_is_untrusted() {
        let a = agreement(&[], 0.7, 0.6);
        assert_eq!(a.n, 0);
        assert!(!a.trusted);
    }
}
