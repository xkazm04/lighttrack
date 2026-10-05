//! Replaying one candidate rule over a series of past instants — the arithmetic behind
//! `GET /v1/limits/preview`.
//!
//! An operator setting a threshold has, until now, had one pre-breach surface: current totals per
//! dimension value. That answers *who is spending*; it cannot answer *what a $5/day cap would have
//! done last week*. So the loop is guess → POST → wait → get paged → adjust, and the registry names
//! it: "a governance UI that can only describe spend after a 429 teaches operators to set thresholds
//! by folklore".
//!
//! This module is the pure half of the answer. The caller hands it the rolling usage a candidate
//! rule would have seen at each sampled instant; it walks them through
//! [`LimitRule::evaluate_resolved_at`] — **the production evaluator, at the replayed clock instant**
//! — and folds the consecutive breached steps into episodes. There is deliberately no second
//! comparison and no second shed formula here: a preview whose ramp is its own arithmetic is a
//! preview of a rule that does not exist.
//!
//! No I/O, so the ramp and the episode folding are unit-testable against hand-built series.

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{CostEvidence, LimitRule, ThresholdBasis};

/// Rolling usage at one sampled instant, as the caller measured it from the store.
#[derive(Debug, Clone)]
pub struct BacktestSample {
    /// The replayed clock instant. The window behind it is the caller's business.
    pub at: DateTime<Utc>,
    /// The metric figure the rule compares against — already `Usage::metric_value`, so a cost cap
    /// sees the imputed charge for unpriced traffic exactly as enforcement would.
    pub current: f64,
    /// Calls inside the window, carried so an episode can say how much traffic it covers.
    pub calls: i64,
    /// Stored cost inside the window, same purpose.
    pub cost_usd: f64,
    /// Cost provenance of `current` for a `cost_usd` rule — `None` for `calls`/`tokens`.
    pub cost_evidence: Option<CostEvidence>,
}

/// One step of the replayed timeline: what the rule would have said at that instant.
#[derive(Debug, Clone, Serialize)]
pub struct BacktestStep {
    pub at: DateTime<Utc>,
    pub current: f64,
    pub ratio: f64,
    /// Share of ingest a `throttle` rule would have been shedding here, in `[0, 1]`. Always `0` for
    /// `alert`/`block`, which never shed before the threshold.
    pub shed_fraction: f64,
    pub warning: bool,
    pub breached: bool,
    pub calls: i64,
    pub cost_usd: f64,
}

/// A run of consecutive breached steps — one incident the cap would have caused.
#[derive(Debug, Clone, Serialize)]
pub struct BacktestEpisode {
    pub first_at: DateTime<Utc>,
    pub last_at: DateTime<Utc>,
    pub peak_ratio: f64,
    /// How many sampled steps the episode spans.
    pub steps: usize,
    /// Calls and cost in the window at the episode's peak — the traffic the cap would have been
    /// turning away, not a sum over overlapping rolling windows (which would double-count).
    pub calls: i64,
    pub cost_usd: f64,
}

/// The replay's verdict.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Backtest {
    /// Whether the candidate would have breached at any sampled instant.
    pub would_breach: bool,
    /// Highest ratio reached across the timeline. `0.0` for an empty timeline.
    pub peak_ratio: f64,
    pub breached_steps: usize,
    pub episodes: Vec<BacktestEpisode>,
    pub timeline: Vec<BacktestStep>,
}

/// Replay `rule` against `samples`, at each sample's own instant.
///
/// `threshold` and `basis` come from the caller's [`Threshold::resolve`](super::Threshold::resolve),
/// so the preview resolves a cap the same way the status surface and the admission path do.
pub fn replay(
    rule: &LimitRule,
    threshold: f64,
    basis: ThresholdBasis,
    samples: &[BacktestSample],
) -> Backtest {
    let mut out = Backtest::default();
    for s in samples {
        let st =
            rule.evaluate_resolved_at(s.current, threshold, basis, s.cost_evidence.clone(), s.at);
        out.timeline.push(BacktestStep {
            at: s.at,
            current: st.current,
            ratio: st.ratio,
            shed_fraction: st.shed_fraction,
            warning: st.warning,
            breached: st.breached,
            calls: s.calls,
            cost_usd: s.cost_usd,
        });
    }
    out.peak_ratio = out
        .timeline
        .iter()
        .map(|s| s.ratio)
        .filter(|r| r.is_finite())
        .fold(0.0_f64, f64::max);
    out.breached_steps = out.timeline.iter().filter(|s| s.breached).count();
    out.would_breach = out.breached_steps > 0;
    out.episodes = episodes(&out.timeline);
    out
}

/// Fold consecutive breached steps into episodes. A gap of one unbreached step closes an episode —
/// the sampling grid is the resolution, and pretending otherwise would merge two incidents into one.
fn episodes(timeline: &[BacktestStep]) -> Vec<BacktestEpisode> {
    let mut out: Vec<BacktestEpisode> = Vec::new();
    let mut open: Option<BacktestEpisode> = None;
    for s in timeline {
        if !s.breached {
            if let Some(e) = open.take() {
                out.push(e);
            }
            continue;
        }
        match &mut open {
            None => {
                open = Some(BacktestEpisode {
                    first_at: s.at,
                    last_at: s.at,
                    peak_ratio: s.ratio,
                    steps: 1,
                    calls: s.calls,
                    cost_usd: s.cost_usd,
                })
            }
            Some(e) => {
                e.last_at = s.at;
                e.steps += 1;
                if s.ratio > e.peak_ratio {
                    e.peak_ratio = s.ratio;
                    e.calls = s.calls;
                    e.cost_usd = s.cost_usd;
                }
            }
        }
    }
    if let Some(e) = open {
        out.push(e);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LimitAction, LimitMetric, LimitRule, LimitWindow, Threshold};
    use chrono::Duration;

    fn rule(action: LimitAction, threshold: f64, warn_at: Option<f64>) -> LimitRule {
        LimitRule {
            id: "preview".into(),
            project_id: "p1".into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Day,
            threshold: Threshold::Fixed(threshold),
            action,
            enabled: true,
            warn_at,
            scope: None,
            escalation: None,
            escalated_until: None,
            origin: None,
            expires_at: None,
        }
    }

    fn series(values: &[f64]) -> Vec<BacktestSample> {
        let t0 = Utc::now() - Duration::days(7);
        values
            .iter()
            .enumerate()
            .map(|(i, v)| BacktestSample {
                at: t0 + Duration::hours(i as i64),
                current: *v,
                calls: 1,
                cost_usd: *v,
                cost_evidence: None,
            })
            .collect()
    }

    /// CASE 3 — the ramp is the production evaluator's arithmetic, not a second formula.
    ///
    /// `action=throttle`, `threshold=5`, `warn_at=0.5` ⇒ `throttle_start = 0.5`, so the step whose
    /// ratio is 0.75 sheds `(0.75 - 0.5) / (1 - 0.5) = 0.5`. If this module restated the ramp it
    /// would drift from `LimitRule::evaluate_resolved_at` and the preview would promise a
    /// back-pressure curve the server does not apply.
    #[test]
    fn the_shed_ramp_is_the_rules_own_arithmetic() {
        let r = rule(LimitAction::Throttle, 5.0, Some(0.5));
        let (threshold, basis) = r.threshold.resolve(None);
        // ratios 0.0, 0.5, 0.75, 1.0
        let bt = replay(&r, threshold, basis, &series(&[0.0, 2.5, 3.75, 5.0]));
        assert_eq!(bt.timeline.len(), 4);
        for s in &bt.timeline {
            assert!(
                (0.0..=1.0).contains(&s.shed_fraction),
                "shed_fraction must stay a fraction: {s:?}"
            );
        }
        let at_075 = bt
            .timeline
            .iter()
            .find(|s| (s.ratio - 0.75).abs() < 1e-9)
            .expect("the 0.75 step is in the timeline");
        assert!(
            (at_075.shed_fraction - 0.5).abs() < 1e-9,
            "the ramp from warn_at=0.5 puts ratio 0.75 at half shed, got {}",
            at_075.shed_fraction
        );
        assert_eq!(bt.timeline[0].shed_fraction, 0.0, "below the ramp: nothing");
        assert_eq!(
            bt.timeline[3].shed_fraction, 1.0,
            "at the threshold shedding has become a hard stop"
        );
        // …and the same series under `block` never sheds at all.
        let b = rule(LimitAction::Block, 5.0, Some(0.5));
        let bt = replay(&b, threshold, basis, &series(&[0.0, 2.5, 3.75, 5.0]));
        assert!(bt.timeline.iter().all(|s| s.shed_fraction == 0.0));
    }

    /// A breach the operator would have been paged for shows up as an episode with a span, and the
    /// headline peak is the timeline's maximum.
    #[test]
    fn consecutive_breached_steps_fold_into_one_episode() {
        let r = rule(LimitAction::Block, 5.0, None);
        let (threshold, basis) = r.threshold.resolve(None);
        let bt = replay(
            &r,
            threshold,
            basis,
            &series(&[1.0, 6.0, 7.2, 6.0, 1.0, 9.0]),
        );
        assert!(bt.would_breach);
        assert!((bt.peak_ratio - 9.0 / 5.0).abs() < 1e-9);
        assert_eq!(bt.breached_steps, 4);
        assert_eq!(
            bt.episodes.len(),
            2,
            "a calm step closes an episode: {bt:?}"
        );
        let first = &bt.episodes[0];
        assert_eq!(first.steps, 3);
        assert!(first.first_at < first.last_at);
        assert!((first.peak_ratio - 7.2 / 5.0).abs() < 1e-9);
        assert!(
            (first.cost_usd - 7.2).abs() < 1e-9,
            "the episode reports the traffic at its peak, not a sum over overlapping windows"
        );
    }

    /// The slack-cap answer: an operator must be able to learn a threshold is too high to ever
    /// bind, and learn it as `would_breach = false` with **no** episodes.
    #[test]
    fn a_slack_threshold_produces_no_episodes_and_a_sub_unit_peak() {
        let r = rule(LimitAction::Block, 100.0, None);
        let (threshold, basis) = r.threshold.resolve(None);
        let bt = replay(&r, threshold, basis, &series(&[1.0, 6.0, 7.2]));
        assert!(!bt.would_breach);
        assert!(bt.episodes.is_empty());
        assert!(bt.peak_ratio < 1.0, "{}", bt.peak_ratio);
        assert_eq!(bt.breached_steps, 0);
    }

    /// An empty series is an empty verdict — never a confident "nothing would have breached" with a
    /// peak of zero dressed up as a measurement. The caller is the one that must refuse (501) when
    /// it could not look; this only has to not invent an answer.
    #[test]
    fn an_empty_series_asserts_nothing() {
        let r = rule(LimitAction::Block, 5.0, None);
        let (threshold, basis) = r.threshold.resolve(None);
        let bt = replay(&r, threshold, basis, &[]);
        assert!(bt.timeline.is_empty());
        assert!(bt.episodes.is_empty());
        assert!(!bt.would_breach);
        assert_eq!(bt.peak_ratio, 0.0);
    }
}
