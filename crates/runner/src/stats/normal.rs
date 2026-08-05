//! Standard-normal primitives the verdict math needs: a CDF (for p-values) and its inverse (for a
//! family-wise-corrected critical value). Kept separate from [`super`] so the distribution code can
//! be checked against published quantiles on its own — every function here is a pure transform with
//! a documented worst-case error, so a reviewer can verify the numbers by hand.

/// Error function, Abramowitz & Stegun 7.1.26. Max absolute error 1.5e-7 — three orders of magnitude
/// finer than any decision we make with it (we compare p-values against α ≈ 0.05 / m).
fn erf(x: f64) -> f64 {
    const P: f64 = 0.327_591_1;
    const A: [f64; 5] = [
        0.254_829_592,
        -0.284_496_736,
        1.421_413_741,
        -1.453_152_027,
        1.061_405_429,
    ];
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + P * x);
    let poly = A.iter().rev().fold(0.0, |acc, &a| (acc + a) * t);
    sign * (1.0 - poly * (-x * x).exp())
}

/// Φ(z) — the standard-normal CDF.
pub(crate) fn norm_cdf(z: f64) -> f64 {
    0.5 * (1.0 + erf(z / std::f64::consts::SQRT_2))
}

/// Two-sided p-value for a z statistic: `P(|Z| ≥ |z|)`.
pub(crate) fn two_sided_p(z: f64) -> f64 {
    (2.0 * (1.0 - norm_cdf(z.abs()))).clamp(0.0, 1.0)
}

/// Φ⁻¹(p) — the standard-normal quantile, via Acklam's rational approximation (relative error
/// < 1.15e-9). Outside `(0, 1)` it saturates rather than returning NaN, so a degenerate α can only
/// make the test *more* conservative, never undefined.
pub(crate) fn z_quantile(p: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_690e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239e0,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838e0,
        -2.549_732_539_343_734e0,
        4.374_664_141_464_968e0,
        2.938_163_982_698_783e0,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996e0,
        3.754_408_661_907_416e0,
    ];
    const P_LOW: f64 = 0.02425;

    if p <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if p >= 1.0 {
        return f64::INFINITY;
    }
    if p < P_LOW {
        let q = (-2.0 * p.ln()).sqrt();
        return (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0);
    }
    if p > 1.0 - P_LOW {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        return -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0);
    }
    let q = p - 0.5;
    let r = q * q;
    (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
        / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
}

/// Critical two-sided z at family-wise α across `m` comparisons, **Bonferroni**: each comparison is
/// tested at `α/m`, so the probability of *any* false positive across the family stays ≤ α.
///
/// Why this matters here: compare mode runs one independent ~95% test per target. At m = 6 targets
/// the chance of at least one spurious `regressed` is `1 − 0.95⁶ ≈ 26%` — a quarter of clean runs
/// would show a red target. Bonferroni is deliberately the conservative choice (it costs power, and
/// the report says so) because a false `regressed` blocks a deploy, which is the expensive error.
pub(crate) fn bonferroni_z(alpha: f64, m: usize) -> f64 {
    let per = alpha / m.max(1) as f64;
    z_quantile(1.0 - per / 2.0)
}

/// The per-comparison α Bonferroni leaves after correcting for `m` comparisons.
pub(crate) fn bonferroni_alpha(alpha: f64, m: usize) -> f64 {
    alpha / m.max(1) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn cdf_matches_published_values() {
        assert!(near(norm_cdf(0.0), 0.5, 1e-8));
        // Φ(1) = 0.8413447, Φ(1.96) = 0.9750021, Φ(2.5758) = 0.9950 (standard tables).
        assert!(near(norm_cdf(1.0), 0.841_344_7, 1e-6));
        assert!(near(norm_cdf(1.959_963_98), 0.975, 1e-6));
        assert!(near(norm_cdf(2.575_829_3), 0.995, 1e-6));
        // Symmetry: Φ(−z) = 1 − Φ(z).
        assert!(near(norm_cdf(-1.3), 1.0 - norm_cdf(1.3), 1e-9));
    }

    #[test]
    fn two_sided_p_matches_worked_examples() {
        // z = 1.96 is the textbook 5% two-sided cut-off.
        assert!(near(two_sided_p(1.959_963_98), 0.05, 1e-5));
        // z = 2.5758 → 1%; z = 0 → nothing to see (p = 1); sign is irrelevant.
        assert!(near(two_sided_p(2.575_829_3), 0.01, 1e-5));
        assert!(near(two_sided_p(0.0), 1.0, 1e-6));
        assert!(near(two_sided_p(-3.0), two_sided_p(3.0), 1e-12));
        // z = 3 → 0.0027 (the "three sigma" figure).
        assert!(near(two_sided_p(3.0), 0.002_7, 1e-4));
    }

    #[test]
    fn quantile_inverts_the_cdf() {
        assert!(near(z_quantile(0.5), 0.0, 1e-9));
        assert!(
            near(z_quantile(0.975), 1.959_963_98, 1e-6),
            "the 95% two-sided z"
        );
        assert!(near(z_quantile(0.995), 2.575_829_3, 1e-6));
        assert!(near(z_quantile(0.025), -1.959_963_98, 1e-6));
        // Round-trip through the CDF at a few points in each region of the approximation.
        for p in [0.001, 0.01, 0.2, 0.5, 0.8, 0.99, 0.999] {
            assert!(
                near(norm_cdf(z_quantile(p)), p, 1e-6),
                "round trip failed at p={p}"
            );
        }
        // Degenerate inputs saturate instead of producing NaN.
        assert!(z_quantile(0.0).is_infinite() && z_quantile(1.0).is_infinite());
    }

    #[test]
    fn bonferroni_widens_the_critical_value_with_the_family() {
        // m = 1 reproduces the uncorrected 95% z exactly — a single-target run is unchanged.
        assert!(near(bonferroni_z(0.05, 1), 1.959_963_98, 1e-6));
        assert!(near(bonferroni_alpha(0.05, 1), 0.05, 1e-12));
        // m = 6 targets → per-comparison α = 0.008333 → two-sided z ≈ 2.6383.
        assert!(near(bonferroni_alpha(0.05, 6), 0.008_333_33, 1e-8));
        assert!(
            near(bonferroni_z(0.05, 6), 2.638_257, 1e-4),
            "got {}",
            bonferroni_z(0.05, 6)
        );
        // m = 2 → α' = 0.025 → z ≈ 2.2414. Monotone: more comparisons ⇒ a stricter bar.
        assert!(near(bonferroni_z(0.05, 2), 2.241_403, 1e-4));
        assert!(bonferroni_z(0.05, 6) > bonferroni_z(0.05, 2));
        assert!(bonferroni_z(0.05, 2) > bonferroni_z(0.05, 1));
        // m = 0 is treated as 1 rather than dividing by zero.
        assert!(near(bonferroni_z(0.05, 0), bonferroni_z(0.05, 1), 1e-12));
    }
}
