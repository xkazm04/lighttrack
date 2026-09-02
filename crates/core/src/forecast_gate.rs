//! The honesty gate in front of [`crate::forecast`]: *may this projection be shown to anyone?*
//!
//! `Trend::fit` never fails. Two points make a slope, and a dense daily series pads absent days with
//! zeros, so a project that started spending three days ago inside a fourteen-day window is fitted
//! over eleven zeros and three real points — steeply "rising" by construction. The arithmetic is
//! right and the conclusion is nonsense. This module is where that is caught:
//!
//! * an **evidence floor** ([`MIN_OBSERVED_DAYS`], [`MIN_SPAN_DAYS`]) a fit must clear before its ETA
//!   is presented at all — below it, [`Trend::presentability`] hands back a [`Refusal`] whose
//!   `reason` is written to be read by an operator, not parsed;
//! * a **flat band** ([`FLAT_BAND`]) around zero, sized relative to the level, so day-to-day noise on
//!   a steady spend is a flat trend and not a breach ETA;
//! * a **burn-rate corroboration** ([`Trend::corroborated`]): the last few days must actually be
//!   running above the window's own baseline before a rising projection may page someone.
//!
//! The gate is advisory-facing but conservative by construction: everything it refuses is reported
//! as a refusal, never as a quiet absence. A forecast surface that silently says nothing when it
//! cannot see is indistinguishable from one that says nothing because all is well.

use serde::Serialize;

use crate::forecast::Trend;

/// Fewest **non-zero** observed days a projection may be presented from. Four is the smallest number
/// at which a linear fit has any residual left to be wrong about: with two points the fit is exact
/// by construction and r² is meaninglessly 1.0.
pub const MIN_OBSERVED_DAYS: usize = 4;

/// Fewest days those observations must span (first non-zero → last non-zero, inclusive). Four
/// points crowded into two adjacent days describe a spike, not a trend.
pub const MIN_SPAN_DAYS: u32 = 4;

/// Half-width of the flat band, as a fraction of the level: a slope whose magnitude is within
/// `FLAT_BAND × level` per day is treated as **flat**. At 5%, a spend that would take three weeks to
/// double is not called a trend — which is the right call for a horizon measured in days.
pub const FLAT_BAND: f64 = 0.05;

/// Why a projection was withheld. `reason` is copy-ready prose: it is rendered straight into the
/// API's `refused[]` and into an operator's terminal, so it says what is missing and how much.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refusal {
    pub reason: String,
}

impl Refusal {
    fn new(reason: impl Into<String>) -> Refusal {
        Refusal {
            reason: reason.into(),
        }
    }
}

impl Trend {
    /// `Ok(())` when this fit clears the evidence floor, else the [`Refusal`] explaining which half
    /// of the floor it missed. Callers that page someone must consult this first.
    pub fn presentability(&self, min_points: usize, min_span_days: u32) -> Result<(), Refusal> {
        if self.n == 0 {
            return Err(Refusal::new("no daily history in the window"));
        }
        if self.n_nonzero < min_points {
            return Err(Refusal::new(format!(
                "{min_points} observed days needed, {} seen",
                self.n_nonzero
            )));
        }
        if self.span_days < min_span_days {
            return Err(Refusal::new(format!(
                "observations span {} day{}, {min_span_days} needed",
                self.span_days,
                if self.span_days == 1 { "" } else { "s" }
            )));
        }
        Ok(())
    }

    /// [`Trend::presentability`] at the default floor — the one the API and the alert path use.
    pub fn is_presentable(&self) -> bool {
        self.presentability(MIN_OBSERVED_DAYS, MIN_SPAN_DAYS)
            .is_ok()
    }

    /// The slope with the flat band applied: `0.0` when `|slope|` is inside `FLAT_BAND × |level|`.
    pub fn effective_slope(&self) -> f64 {
        if self.slope.abs() <= FLAT_BAND * self.level.abs() {
            0.0
        } else {
            self.slope
        }
    }

    /// Whether the *live* burn rate corroborates a rising projection: the last few days' mean is
    /// above the window's own baseline mean.
    ///
    /// Corroboration is deliberately against the window mean rather than the EWMA `level`. The EWMA
    /// is itself weighted toward the newest points, so on any genuinely rising series it sits just
    /// *above* the trailing mean — comparing the two would suppress exactly the alerts worth
    /// sending. The baseline is the honest reference: "these last days really are hotter than the
    /// period we fitted", which is what rules out an ETA carried by an old spike.
    pub fn corroborated(&self) -> bool {
        self.recent_mean > self.window_mean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_two_point_perfect_fit_is_refused() {
        let t = Trend::fit(&[1.0, 9.0]);
        assert!(t.slope > 0.0, "the arithmetic still produces a slope");
        let r = t
            .presentability(MIN_OBSERVED_DAYS, MIN_SPAN_DAYS)
            .unwrap_err();
        assert_eq!(r.reason, "4 observed days needed, 2 seen");
        assert!(
            t.r2.is_none(),
            "no confidence is published for a refused fit"
        );
    }

    #[test]
    fn a_young_project_padded_with_leading_zeros_no_longer_reads_as_rising() {
        // Fourteen-day window, spend started three days ago and is perfectly flat since.
        let mut series = vec![0.0; 11];
        series.extend([5.0, 5.0, 5.0]);
        let t = Trend::fit(&series);
        assert!(
            t.slope > 0.0,
            "zero-fill makes the raw slope positive — that is the defect being gated"
        );
        assert_eq!(t.n, 14);
        assert_eq!(t.n_nonzero, 3);
        assert_eq!(t.span_days, 3);
        assert!(t.presentability(MIN_OBSERVED_DAYS, MIN_SPAN_DAYS).is_err());
        assert!(t.r2.is_none());
    }

    #[test]
    fn a_short_span_is_refused_even_with_enough_points() {
        // Four non-zero days, but crowded into three: a spike, not a trend.
        let t = Trend::fit(&[0.0, 0.0, 1.0, 2.0, 3.0]);
        assert_eq!(t.n_nonzero, 3);
        let r = t.presentability(2, MIN_SPAN_DAYS).unwrap_err();
        assert_eq!(r.reason, "observations span 3 days, 4 needed");
    }

    #[test]
    fn an_established_series_is_presentable_and_publishes_confidence() {
        let t = Trend::fit(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert!(t.presentability(MIN_OBSERVED_DAYS, MIN_SPAN_DAYS).is_ok());
        assert_eq!(t.span_days, 6);
        let r2 = t.r2.expect("a presentable fit publishes its r²");
        assert!(r2 > 0.99, "a perfect line fits perfectly: {r2}");
    }

    #[test]
    fn a_slope_inside_the_flat_band_is_flat() {
        // ~$100/day drifting by well under 5% of the level per day: noise, not a trend.
        let t = Trend::fit(&[100.0, 101.0, 100.0, 102.0, 101.0, 102.0]);
        assert!(t.slope > 0.0);
        assert_eq!(t.effective_slope(), 0.0);
        assert!(
            t.days_until_daily(150.0, 30).is_none(),
            "a flat trend never reaches a higher threshold"
        );
        // Outside the band, the same shape does forecast a crossing.
        let steep = Trend::fit(&[10.0, 30.0, 50.0, 70.0, 90.0, 110.0]);
        assert!(steep.effective_slope() > 0.0);
        assert!(steep.days_until_daily(150.0, 30).is_some());
    }

    #[test]
    fn corroboration_needs_the_recent_days_above_the_baseline() {
        assert!(Trend::fit(&[1.0, 2.0, 3.0, 4.0, 5.0]).corroborated());
        // An old spike that has since cooled: the fit may still slope, the burn rate does not agree.
        assert!(!Trend::fit(&[40.0, 30.0, 20.0, 10.0, 5.0]).corroborated());
        assert!(!Trend::fit(&[5.0, 5.0, 5.0, 5.0]).corroborated());
    }
}
