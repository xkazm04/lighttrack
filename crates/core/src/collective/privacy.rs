//! Privacy treatments applied to published digest fields.
//!
//! Quality, pass-rate and task type are already coarse by construction (bounded scales, a fixed task
//! vocabulary, a k-anonymity floor over cases). **Cost is not**: `avg_cost_usd` is an unbounded
//! continuous number derived from one instance's exact pricing, provider mix and prompt lengths, so a
//! distinctive cost-per-case identifies a contributor even when its quality and `n_cases` clear both
//! floors. It gets the treatment the other fields get for free.

/// Round a published cost to **2 significant figures**.
///
/// The honest statement of what this buys: a cost stops being a unique continuous fingerprint and
/// becomes a band ~1–10% wide that many contributors share, while staying precise enough to rank
/// models on price (`$0.0031` and `$0.0034` still separate). It is *not* anonymity on its own — the
/// k-anonymity floors over cases and sources are what make a bucket unattributable; this removes the
/// side channel that would have defeated them.
pub fn bucket_cost(x: f64) -> f64 {
    if !x.is_finite() || x <= 0.0 {
        return 0.0;
    }
    let scale = 10f64.powf(x.abs().log10().floor() - 1.0);
    let bucketed = (x / scale).round() * scale;
    // Kill the float representation noise the divide/multiply introduces (…0.0030000000000000005).
    let places = (2.0 - x.abs().log10().floor()).clamp(0.0, 15.0) as i32;
    let p = 10f64.powi(places);
    (bucketed * p).round() / p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_two_significant_figures_across_magnitudes() {
        assert_eq!(bucket_cost(0.003_142), 0.0031);
        assert_eq!(bucket_cost(0.003_47), 0.0035);
        assert_eq!(bucket_cost(1.234), 1.2);
        assert_eq!(bucket_cost(0.000_001_234), 0.0000012);
        assert_eq!(bucket_cost(97.0), 97.0);
        assert_eq!(bucket_cost(0.0), 0.0);
        assert_eq!(bucket_cost(-1.0), 0.0);
        assert_eq!(bucket_cost(f64::NAN), 0.0);
    }

    #[test]
    fn distinct_costs_collapse_into_a_shared_band() {
        // Three instances whose true per-case costs differ in the 3rd figure become indistinguishable.
        let band: std::collections::BTreeSet<u64> = [0.003_10, 0.003_13, 0.003_14]
            .iter()
            .map(|c| (bucket_cost(*c) * 1e9) as u64)
            .collect();
        assert_eq!(band.len(), 1, "the fingerprint is gone");
        // …while a genuinely different price point stays visible.
        assert_ne!(bucket_cost(0.0031), bucket_cost(0.0042));
    }
}
