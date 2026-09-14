//! Accumulating one compare cell's generation spend, candidate by candidate.
//!
//! A cell generates `--gen-samples` candidates, and each reports tokens, perhaps a reasoning split, a
//! latency and a cost that may or may not be known. The rule this module holds: **an unknown is never
//! a zero**. One candidate with no reasoning split makes the cell's reasoning figure unknown, rather
//! than an average that quietly counts it as a call that did not think.

use lighttrack_core::GenerationFacts;
use lighttrack_engine::GenOutcome;

/// Which count stands for "how hard the model thought".
///
/// Reasoning tokens where every call reported the split; output tokens otherwise. On a short-answer
/// task output tokens are nearly all thinking, which makes them a usable proxy for the providers that
/// report no split (Anthropic, `claude -p`) — but they are never *labelled* reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThinkingBasis {
    Reasoning,
    Output,
}

impl ThinkingBasis {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ThinkingBasis::Reasoning => "reasoning_tokens",
            ThinkingBasis::Output => "output_tokens",
        }
    }
}

/// A running sum that becomes unknown the moment one contribution is.
#[derive(Debug, Clone, Copy, Default)]
struct Tally {
    sum: f64,
    unknown: bool,
}

impl Tally {
    fn add(&mut self, v: Option<f64>) {
        match v {
            Some(x) => self.sum += x,
            None => self.unknown = true,
        }
    }

    fn absorb(&mut self, other: Tally) {
        self.sum += other.sum;
        self.unknown |= other.unknown;
    }
}

/// The generation spend of one cell (or, absorbed, of a whole target).
#[derive(Debug, Clone, Default)]
pub(crate) struct GenSpend {
    calls: u32,
    output: Tally,
    reasoning: Tally,
    latency: Tally,
    cost: Tally,
}

impl GenSpend {
    /// Record one candidate that generated. `cost_usd` is what the provider or the price book said,
    /// and `priced` whether either of them knew — an unpriced call contributes an unknown, not `$0`.
    pub(crate) fn record(&mut self, g: &GenOutcome, cost_usd: f64, priced: bool) {
        self.calls += 1;
        self.output.add(g.output_tokens.map(|t| t as f64));
        self.reasoning.add(g.reasoning_tokens.map(|t| t as f64));
        self.latency.add(g.latency_ms.map(|t| t as f64));
        self.cost.add(priced.then_some(cost_usd));
    }

    pub(crate) fn absorb(&mut self, other: &GenSpend) {
        self.calls += other.calls;
        self.output.absorb(other.output);
        self.reasoning.absorb(other.reasoning);
        self.latency.absorb(other.latency);
        self.cost.absorb(other.cost);
    }

    fn total(&self, t: Tally) -> Option<f64> {
        (self.calls > 0 && !t.unknown).then_some(t.sum)
    }

    fn mean(&self, t: Tally) -> Option<f64> {
        self.total(t).map(|s| s / self.calls as f64)
    }

    /// Per-candidate means for the verdict's detail, or `None` when nothing generated.
    pub(crate) fn facts(&self) -> Option<GenerationFacts> {
        (self.calls > 0).then(|| GenerationFacts {
            candidates: self.calls,
            output_tokens: self.mean(self.output),
            reasoning_tokens: self.mean(self.reasoning),
            latency_ms: self.mean(self.latency),
            cost_usd: self.mean(self.cost),
        })
    }

    /// Whole-target totals for the run report: output tokens, reasoning tokens, and the basis a
    /// thinking measure over this spend has to use.
    pub(crate) fn totals(&self) -> (Option<u64>, Option<u64>, Option<ThinkingBasis>) {
        let output = self.total(self.output).map(|x| x as u64);
        let reasoning = self.total(self.reasoning).map(|x| x as u64);
        let basis = match (reasoning, output) {
            (Some(_), _) => Some(ThinkingBasis::Reasoning),
            (None, Some(_)) => Some(ThinkingBasis::Output),
            (None, None) => None,
        };
        (output, reasoning, basis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_engine::{Determinism, SchemaEnforcement};

    fn gen(output: Option<u64>, reasoning: Option<u64>, latency: Option<u64>) -> GenOutcome {
        GenOutcome {
            output: "x".into(),
            cost_usd: None,
            model: "m".into(),
            latency_ms: latency,
            input_tokens: Some(10),
            output_tokens: output,
            reasoning_tokens: reasoning,
            determinism: Determinism::BestEffort,
            schema: SchemaEnforcement::NotRequested,
        }
    }

    #[test]
    fn facts_are_per_candidate_means() {
        let mut s = GenSpend::default();
        s.record(&gen(Some(1000), Some(900), Some(4000)), 0.02, true);
        s.record(&gen(Some(500), Some(400), Some(2000)), 0.01, true);
        let f = s.facts().unwrap();
        assert_eq!(f.candidates, 2);
        assert_eq!(f.output_tokens, Some(750.0));
        assert_eq!(f.reasoning_tokens, Some(650.0));
        assert_eq!(f.latency_ms, Some(3000.0));
        assert!((f.cost_usd.unwrap() - 0.015).abs() < 1e-12);
        assert_eq!(
            s.totals(),
            (Some(1500), Some(1300), Some(ThinkingBasis::Reasoning))
        );
    }

    /// **The rule.** One candidate without a split makes the cell's reasoning unknown — averaging it
    /// in as zero would halve the thinking a model visibly did.
    #[test]
    fn one_unknown_contribution_makes_the_figure_unknown_not_smaller() {
        let mut s = GenSpend::default();
        s.record(&gen(Some(1000), Some(900), Some(10)), 0.02, true);
        s.record(&gen(Some(1000), None, Some(10)), 0.0, false);
        let f = s.facts().unwrap();
        assert_eq!(f.reasoning_tokens, None);
        assert_eq!(f.cost_usd, None, "an unpriced call is not a free one");
        assert_eq!(f.output_tokens, Some(1000.0));
        // With no reasoning split, output tokens are the basis — and are named as such.
        assert_eq!(s.totals(), (Some(2000), None, Some(ThinkingBasis::Output)));
    }

    #[test]
    fn absorbing_cells_keeps_the_unknowns_they_carried() {
        let mut a = GenSpend::default();
        a.record(&gen(Some(100), Some(50), Some(1)), 0.01, true);
        let mut b = GenSpend::default();
        b.record(&gen(Some(300), None, None), 0.03, true);
        let mut target = GenSpend::default();
        target.absorb(&a);
        target.absorb(&b);
        assert_eq!(
            target.totals(),
            (Some(400), None, Some(ThinkingBasis::Output))
        );
        assert_eq!(target.facts().unwrap().latency_ms, None);
        // Nothing generated: no facts at all, rather than a row of zeroes.
        assert!(GenSpend::default().facts().is_none());
        assert_eq!(GenSpend::default().totals(), (None, None, None));
    }
}
