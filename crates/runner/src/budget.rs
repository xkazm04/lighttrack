//! Per-run spend control for benchmark **compare** runs: a pre-flight estimate printed before the
//! first paid call, and a live ceiling that halts the run at a case boundary.
//!
//! This is an *operator ceiling on one benchmark run*, deliberately separate from the ingest limit
//! engine: the judge/scoring engine stays unbudgeted by repo invariant. Nothing here talks to
//! `limit_rules`, and exceeding the ceiling never blocks ingest — it stops spending on this run and
//! marks the results partial.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use lighttrack_core::{BenchTarget, ModelPriceRow};

use crate::util::price_gen_cost_checked;

/// Dollars are accumulated as integer micro-dollars so concurrent cells can add to one atomic
/// counter without a lock (and without float races); $1e-6 is far below any per-call cost.
const MICROS: f64 = 1_000_000.0;

/// Nominal tokens per call used by the pre-flight estimate. A benchmark's real prompts are unknown
/// before it runs, so this is an ORDER-OF-MAGNITUDE figure — it exists to catch a matrix that is
/// 100× too expensive, not to predict the invoice. The live [`Budget`] enforces the real number.
const EST_GEN_IN: u64 = 1_000;
const EST_GEN_OUT: u64 = 500;
const EST_JUDGE_IN: u64 = 1_500;
const EST_JUDGE_OUT: u64 = 400;

/// A compare matrix's pre-flight cost estimate.
pub(crate) struct CostEstimate {
    pub(crate) gen_calls: usize,
    pub(crate) judge_calls: usize,
    pub(crate) usd: f64,
    /// Models with no price-book entry: their share of the estimate is $0, so `usd` is a LOWER
    /// bound whenever this is non-empty. Reported up front instead of only in a nested array.
    pub(crate) unpriced: BTreeSet<String>,
}

impl CostEstimate {
    /// One line for the pre-flight log.
    pub(crate) fn line(&self) -> String {
        let dollars = if self.unpriced.is_empty() {
            format!("~${:.2}", self.usd)
        } else {
            format!("≥${:.2} (unpriced models excluded)", self.usd)
        };
        format!(
            "cost pre-flight: {} generation + {} judge call(s) ⇒ {dollars} at a nominal \
             {EST_GEN_IN}/{EST_GEN_OUT} tokens per generation and {EST_JUDGE_IN}/{EST_JUDGE_OUT} \
             per judge call",
            self.gen_calls, self.judge_calls,
        )
    }
}

/// Estimate a compare run's cost BEFORE the first paid call: every target generates `ng` candidates
/// per case, and each candidate is judged `samples` times.
pub(crate) fn estimate_compare(
    prices: &[ModelPriceRow],
    targets: &[BenchTarget],
    n_cases: usize,
    ng: u32,
    samples: u32,
    jp: &str,
    jm: &str,
) -> CostEstimate {
    let per_target_gen = n_cases * ng.max(1) as usize;
    let judge_calls = per_target_gen * targets.len() * samples.max(1) as usize;
    let mut unpriced: BTreeSet<String> = BTreeSet::new();
    let mut usd = 0.0;
    for t in targets {
        let (c, priced) = price_gen_cost_checked(
            prices,
            &t.provider,
            &t.model,
            Some(EST_GEN_IN),
            Some(EST_GEN_OUT),
        );
        if !priced {
            unpriced.insert(format!("{}/{}", t.provider, t.model));
        }
        usd += c * per_target_gen as f64;
    }
    let (jc, judge_priced) =
        price_gen_cost_checked(prices, jp, jm, Some(EST_JUDGE_IN), Some(EST_JUDGE_OUT));
    if !judge_priced {
        unpriced.insert(format!("{jp}/{jm}"));
    }
    usd += jc * judge_calls as f64;
    CostEstimate {
        gen_calls: per_target_gen * targets.len(),
        judge_calls,
        usd,
        unpriced,
    }
}

/// A live per-run dollar ceiling. Cells check [`Budget::exhausted`] at a case boundary *before*
/// making a paid call and record what they spent afterwards, so a run whose real cost outruns the
/// pre-flight estimate stops instead of quietly finishing the invoice.
pub(crate) struct Budget {
    limit_micros: Option<u64>,
    spent_micros: AtomicU64,
    halted: AtomicBool,
}

impl Budget {
    /// `limit_usd <= 0` (or non-finite) disables the ceiling.
    pub(crate) fn new(limit_usd: f64) -> Self {
        let limit_micros =
            (limit_usd.is_finite() && limit_usd > 0.0).then(|| (limit_usd * MICROS).round() as u64);
        Budget {
            limit_micros,
            spent_micros: AtomicU64::new(0),
            halted: AtomicBool::new(false),
        }
    }

    pub(crate) fn limit_usd(&self) -> Option<f64> {
        self.limit_micros.map(|m| m as f64 / MICROS)
    }

    /// Record a cell's actual spend.
    pub(crate) fn spend(&self, usd: f64) {
        if !usd.is_finite() || usd <= 0.0 {
            return;
        }
        self.spent_micros
            .fetch_add((usd * MICROS).round() as u64, Ordering::Relaxed);
    }

    pub(crate) fn spent_usd(&self) -> f64 {
        self.spent_micros.load(Ordering::Relaxed) as f64 / MICROS
    }

    /// True once the ceiling has been reached. Checked before starting a unit of work, never
    /// mid-call: stopping at a case boundary is the contract.
    pub(crate) fn exhausted(&self) -> bool {
        match self.limit_micros {
            None => false,
            Some(limit) => self.spent_micros.load(Ordering::Relaxed) >= limit,
        }
    }

    /// Mark that at least one unit of work was skipped because the ceiling was hit. This — not the
    /// spend figure — is what makes a run report as PARTIAL.
    pub(crate) fn halt(&self) {
        self.halted.store(true, Ordering::Relaxed);
    }

    pub(crate) fn halted(&self) -> bool {
        self.halted.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn price(provider: &str, model: &str, i: f64, o: f64) -> ModelPriceRow {
        serde_json::from_value(json!({
            "provider": provider, "model": model, "input_per_mtok": i, "output_per_mtok": o,
        }))
        .unwrap()
    }

    fn target(provider: &str, model: &str) -> BenchTarget {
        serde_json::from_value(json!({ "provider": provider, "model": model })).unwrap()
    }

    #[test]
    fn estimate_counts_every_paid_call_in_the_matrix() {
        let e = estimate_compare(
            &[],
            &[target("a", "m"), target("b", "m")],
            200,
            10,
            2,
            "o",
            "j",
        );
        // 2 targets × 200 cases × 10 candidates = 4000 generations…
        assert_eq!(e.gen_calls, 4000);
        // …each judged twice = 8000 judge calls. This is the fat-finger case the gate exists for.
        assert_eq!(e.judge_calls, 8000);
    }

    #[test]
    fn estimate_prices_from_the_book_and_flags_misses() {
        let prices = vec![
            price("a", "m", 1_000.0, 1_000.0),
            price("o", "j", 1_000.0, 1_000.0),
        ];
        // $1000/Mtok both ways ⇒ a 1k-in/0.5k-out generation is $1.5; a 1.5k/0.4k judge call $1.9.
        let e = estimate_compare(&prices, &[target("a", "m")], 2, 1, 1, "o", "j");
        assert!(
            (e.usd - (2.0 * 1.5 + 2.0 * 1.9)).abs() < 1e-9,
            "got {}",
            e.usd
        );
        assert!(e.unpriced.is_empty());
        // An unpriced target contributes $0 — so the estimate is a LOWER bound and says so.
        let e = estimate_compare(&prices, &[target("zz", "yy")], 2, 1, 1, "o", "j");
        assert_eq!(
            e.unpriced.iter().cloned().collect::<Vec<_>>(),
            vec!["zz/yy".to_string()]
        );
        assert!(
            e.line().contains("≥$"),
            "an unpriced matrix must not print a tilde estimate"
        );
    }

    #[test]
    fn budget_zero_or_negative_is_unlimited() {
        for l in [0.0, -1.0, f64::NAN] {
            let b = Budget::new(l);
            b.spend(1_000_000.0);
            assert!(!b.exhausted(), "limit {l} must disable the ceiling");
            assert_eq!(b.limit_usd(), None);
        }
    }

    #[test]
    fn budget_exhausts_at_the_ceiling_and_records_the_halt() {
        let b = Budget::new(1.0);
        b.spend(0.4);
        assert!(!b.exhausted());
        assert!((b.spent_usd() - 0.4).abs() < 1e-9);
        b.spend(0.6);
        assert!(
            b.exhausted(),
            "spend == limit is exhausted (the next case would overshoot)"
        );
        // Exhausted alone isn't "partial" — a run that spent its last cent on the LAST case is
        // complete. Only skipping work marks it.
        assert!(!b.halted());
        b.halt();
        assert!(b.halted());
    }

    #[test]
    fn budget_is_safe_under_concurrent_cells() {
        // 32 threads each spending $0.10 against a $1 ceiling: the counter must not race, and once
        // the ceiling is crossed every later check must agree.
        let b = Budget::new(1.0);
        std::thread::scope(|s| {
            for _ in 0..32 {
                s.spawn(|| {
                    if !b.exhausted() {
                        b.spend(0.10);
                    } else {
                        b.halt();
                    }
                });
            }
        });
        assert!(b.exhausted());
        assert!(b.halted(), "at least one cell must have been skipped");
        // Every dollar is accounted for: 10 spends land before the ceiling, and each thread spends
        // at most once, so the total is a multiple of $0.10 in [$1.00, $3.20].
        let spent = b.spent_usd();
        assert!((1.0..=3.2).contains(&spent), "got {spent}");
    }
}
