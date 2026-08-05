//! **Between-source heterogeneity** — the half of a merged row's uncertainty the old fixed-effect
//! pooling threw away.
//!
//! The bug this fixes is that the arithmetic ran backwards. Pooling every contributor's cases into one
//! sample and dividing by `N` made the interval shrink with total evidence *regardless of whether the
//! contributors agreed*, so **five sources that disagree got a narrower interval than five that
//! agree** — the exact opposite of what a reader takes "±" to mean. The merge documented the omission
//! honestly, but the number was still printed as the row's uncertainty.
//!
//! ## The estimator
//! Given the row's per-source effective weights `wᵢ` (the *winsorized* case counts, so a whale cannot
//! dominate the spread any more than it dominates the mean) and per-source mean qualities `qᵢ`, with
//! `pᵢ = wᵢ / Σw`:
//!
//! ```text
//! q̄   = Σ pᵢ·qᵢ                       the row's point estimate
//! τ̂²  = Σ pᵢ·(qᵢ − q̄)²  ·  k/(k−1)     between-source variance, Bessel-corrected
//! SE_between² = τ̂² · Σpᵢ²              variance of a weighted mean of k source means
//! ```
//!
//! and the published half-width becomes `1.96·√(SE_within² + SE_between²)` — a random-effects style
//! interval where the within term is the existing pooled case-level variance. With equal weights
//! `Σpᵢ² = 1/k`, so the between term is the familiar `τ̂²/k`.
//!
//! ## What it does at small k — said plainly, not papered over
//! - **k = 1**: `τ̂²` is undefined and reported as `None`; the interval falls back to the within-source
//!   term alone. A one-source row has no between-source evidence *by construction*, not "no
//!   disagreement" — and such a row is normally withheld by the hub's `min_contributors` floor anyway.
//! - **k = 2**: one degree of freedom. `k/(k−1) = 2`, so the correction doubles the raw spread, and two
//!   sources that happen to agree still yield `τ̂² = 0`. Treat a two-source interval as a **lower
//!   bound** on the true uncertainty: it cannot distinguish "these two agree" from "these two agree by
//!   luck". This is why the spread is published alongside the interval rather than folded invisibly
//!   into it.
//! - Growing k stabilizes `τ̂²` in the usual way; nothing here pretends otherwise.
//!
//! ## Why not DerSimonian–Laird
//! The textbook random-effects estimator needs a per-source *within* variance to subtract from the
//! observed dispersion. Roughly half of contributions carry no variance at all (v1 digests, single-run
//! buckets), so DL's `Q` statistic would be computed from a mixture of known and assumed-zero
//! variances — precision theatre over data that does not support it. The estimator above needs only
//! the source means, which **every** contribution has, including v1 ones. It slightly *over*-states the
//! between component (part of the observed spread between source means is really within-source
//! sampling noise, counted twice). We take that direction deliberately: on a public leaderboard, an
//! interval that is a little too wide is a smaller lie than one that is too narrow.

/// The between-source term of a row's uncertainty.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Between {
    /// `τ̂²` — Bessel-corrected weighted variance of the per-source mean qualities. `None` when the row
    /// has fewer than two sources (undefined, not zero).
    pub(crate) tau2: Option<f64>,
    /// `Σpᵢ²` over the normalized weights — `1/k` at equal weights, larger when one source dominates.
    pub(crate) sum_p2: f64,
}

impl Between {
    /// The between-source contribution to the squared standard error of the row's mean. `0.0` when the
    /// row has one source: no between-source evidence exists, so none is invented.
    pub(crate) fn se2(&self) -> f64 {
        self.tau2.unwrap_or(0.0) * self.sum_p2
    }

    /// The weighted standard deviation across sources — the row's *visible* disagreement, published
    /// whether or not a CI could be formed.
    pub(crate) fn spread(&self) -> Option<f64> {
        self.tau2.map(f64::sqrt)
    }
}

/// Compute the between-source term from one row's per-source weights and mean qualities. The slices
/// are parallel; zero/negative total weight yields an empty term.
pub(crate) fn between_sources(weights: &[f64], qualities: &[f64]) -> Between {
    let k = weights.len().min(qualities.len());
    let total: f64 = weights.iter().take(k).sum();
    if k == 0 || total <= 0.0 {
        return Between::default();
    }
    let p: Vec<f64> = weights.iter().take(k).map(|w| w / total).collect();
    let sum_p2 = p.iter().map(|x| x * x).sum();
    if k < 2 {
        return Between { tau2: None, sum_p2 };
    }
    let mean: f64 = p.iter().zip(qualities).map(|(pi, q)| pi * q).sum();
    let raw: f64 = p
        .iter()
        .zip(qualities)
        .map(|(pi, q)| pi * (q - mean).powi(2))
        .sum();
    let corrected = raw * k as f64 / (k as f64 - 1.0);
    Between {
        tau2: Some(corrected.max(0.0)),
        sum_p2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn agreeing_sources_contribute_nothing() {
        // Two equal sources at the same quality: no disagreement, no widening.
        let b = between_sources(&[100.0, 100.0], &[0.82, 0.82]);
        approx(b.tau2.unwrap(), 0.0);
        approx(b.se2(), 0.0);
        approx(b.spread().unwrap(), 0.0);
    }

    #[test]
    fn two_equal_sources_are_hand_checkable() {
        // p = 0.5 each, q̄ = 0.82, raw τ² = 0.5·0.02² + 0.5·0.02² = 0.0004,
        // Bessel k/(k−1) = 2 ⇒ τ̂² = 0.0008; Σp² = 0.5 ⇒ SE² = 0.0004, SE = 0.02.
        let b = between_sources(&[100.0, 100.0], &[0.80, 0.84]);
        approx(b.tau2.unwrap(), 0.0008);
        approx(b.sum_p2, 0.5);
        approx(b.se2(), 0.0004);
        approx(b.spread().unwrap(), 0.0008_f64.sqrt());
    }

    #[test]
    fn wider_disagreement_widens_the_term_quadratically() {
        // Same n, six times the gap (0.12 vs 0.02 per side) ⇒ 36× the variance.
        let near = between_sources(&[100.0, 100.0], &[0.80, 0.84]);
        let far = between_sources(&[100.0, 100.0], &[0.70, 0.94]);
        approx(far.tau2.unwrap(), 0.0288);
        approx(far.se2(), 0.0144);
        assert!((far.se2() / near.se2() - 36.0).abs() < 1e-6);
    }

    #[test]
    fn one_source_is_undefined_not_zero_disagreement() {
        let b = between_sources(&[500.0], &[0.9]);
        assert!(b.tau2.is_none(), "k=1 has no between-source evidence");
        assert!(b.spread().is_none(), "…and nothing to display");
        approx(b.se2(), 0.0);
        approx(b.sum_p2, 1.0);
    }

    #[test]
    fn unequal_weights_shrink_the_effective_source_count() {
        // 80/20 split: Σp² = 0.68 > 1/2, so a dominated row gets LESS averaging-down of the between
        // term than two balanced sources would — one loud source cannot buy precision.
        let b = between_sources(&[80.0, 20.0], &[0.9, 0.5]);
        approx(b.sum_p2, 0.68);
        // q̄ = 0.82; raw τ² = 0.8·0.08² + 0.2·0.32² = 0.00512 + 0.02048 = 0.0256; ×2 = 0.0512.
        approx(b.tau2.unwrap(), 0.0512);
        approx(b.se2(), 0.0512 * 0.68);
    }

    #[test]
    fn three_agreeing_sources_beat_three_disagreeing_ones() {
        let agree = between_sources(&[10.0, 10.0, 10.0], &[0.8, 0.8, 0.8]);
        let disagree = between_sources(&[10.0, 10.0, 10.0], &[0.6, 0.8, 1.0]);
        approx(agree.se2(), 0.0);
        // Σp² = 1/3; raw τ² = (0.04 + 0 + 0.04)/3 = 0.026666…; ×3/2 = 0.04 ⇒ SE² = 0.013333…
        approx(disagree.tau2.unwrap(), 0.04);
        approx(disagree.se2(), 0.04 / 3.0);
        assert!(disagree.se2() > agree.se2());
    }

    #[test]
    fn degenerate_inputs_are_inert() {
        assert!(between_sources(&[], &[]).tau2.is_none());
        assert!(between_sources(&[0.0, 0.0], &[0.5, 0.9]).tau2.is_none());
    }
}
