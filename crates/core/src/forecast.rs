//! Predictive cost/margin forecasting — the pure, I/O-free projection behind the predictive
//! surface. It turns the rolling daily counters the system already keeps (spend, calls, tokens per
//! project; cost per customer/product) into a forward look: *will this project breach its budget,
//! and when?* and *is this customer trending unprofitable?*
//!
//! The model is deliberately small and explainable: an **EWMA level** (a recency-weighted estimate
//! of the current daily value) plus a **least-squares linear slope** (the day-over-day trend), then
//! `value(t) = level + slope·t`. No hidden state, no training — just the same arithmetic an operator
//! would do by eye, made precise. Forecasts are advisory; the alerts they drive say "about N days",
//! not a guarantee.

use serde::Serialize;

use crate::limits::{LimitMetric, LimitRule, LimitWindow};

/// Default EWMA smoothing factor: responsive to the last few days while still smoothing noise.
pub const DEFAULT_ALPHA: f64 = 0.5;

/// An EWMA level + least-squares linear trend fit over an evenly-spaced (one-value-per-day) series,
/// ordered oldest→newest. `level` is the smoothed value at the last (most recent) observation.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Trend {
    /// EWMA of the series — the smoothed "current" daily value.
    pub level: f64,
    /// Least-squares slope per day (change in the daily value per day); `0.0` with <2 points.
    pub slope: f64,
    /// Number of observations the fit used.
    pub n: usize,
}

impl Trend {
    /// Fit with [`DEFAULT_ALPHA`].
    pub fn fit(series: &[f64]) -> Trend {
        Self::fit_with(series, DEFAULT_ALPHA)
    }

    /// Fit an EWMA level (smoothing factor `alpha`, clamped to `(0,1]`) and a linear slope over the
    /// oldest→newest daily `series`.
    pub fn fit_with(series: &[f64], alpha: f64) -> Trend {
        let n = series.len();
        if n == 0 {
            return Trend {
                level: 0.0,
                slope: 0.0,
                n: 0,
            };
        }
        let alpha = alpha.clamp(f64::EPSILON, 1.0);
        let mut ewma = series[0];
        for &x in &series[1..] {
            ewma = alpha * x + (1.0 - alpha) * ewma;
        }
        Trend {
            level: ewma,
            slope: least_squares_slope(series),
            n,
        }
    }

    /// Projected daily value `steps` days after the last observation (never negative — spend and
    /// usage can't go below zero, so a steeply falling trend flattens at the floor).
    pub fn project(&self, steps: f64) -> f64 {
        (self.level + self.slope * steps).max(0.0)
    }

    /// Cumulative projected total over the next `days` whole days (`1..=days`).
    pub fn project_cumulative(&self, days: u32) -> f64 {
        (1..=days).map(|d| self.project(d as f64)).sum()
    }

    /// Smallest day offset `d ≥ 0` (capped at `horizon`) at which the projected **daily** value
    /// reaches `threshold`. `Some(0.0)` when already at/above it; `None` when a flat/falling trend
    /// never reaches it within the horizon.
    pub fn days_until_daily(&self, threshold: f64, horizon: u32) -> Option<f64> {
        if self.level >= threshold {
            return Some(0.0);
        }
        if self.slope <= 0.0 {
            return None;
        }
        let d = (threshold - self.level) / self.slope;
        (d <= horizon as f64).then_some(d)
    }

    /// Smallest (possibly fractional) day count at which **cumulative** projected spend reaches
    /// `budget`, scanned day-by-day to `horizon` and linearly interpolated within the crossing day.
    /// `Some(0.0)` for a non-positive budget (headroom already exhausted); `None` if not reached
    /// within the horizon.
    pub fn days_until_cumulative(&self, budget: f64, horizon: u32) -> Option<f64> {
        if budget <= 0.0 {
            return Some(0.0);
        }
        let mut cum = 0.0;
        for d in 1..=horizon {
            let day = self.project(d as f64);
            if cum + day >= budget {
                let frac = if day > 0.0 { (budget - cum) / day } else { 0.0 };
                return Some((d - 1) as f64 + frac);
            }
            cum += day;
        }
        None
    }
}

/// Forward look for one budget (a cost/calls/tokens [`LimitRule`]) given its metric's daily trend.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetForecast {
    pub rule_id: String,
    pub metric: LimitMetric,
    pub window: LimitWindow,
    pub threshold: f64,
    /// Current rolling value over the rule's window (the live `usage_since` figure).
    pub current: f64,
    /// Projected value for the next day.
    pub projected_daily: f64,
    pub trend: Trend,
    /// Days until the rule is forecast to breach, or `None` if not within the horizon. Present only
    /// for a *future* breach — an already-breached rule is the live-alert path's job, not a forecast.
    pub eta_days: Option<f64>,
}

/// Forecast one budget. A `Day`-window rule breaches when the projected **daily** value reaches the
/// threshold; a `Month`-window rule breaches when cumulative projected spend exhausts the remaining
/// headroom (`threshold − current`) — a conservative roll-off-free estimate that errs early. `Hour`
/// windows are sub-daily, so a daily trend can't forecast them (`eta = None`).
pub fn forecast_budget(
    rule: &LimitRule,
    series: &[f64],
    current: f64,
    horizon: u32,
) -> BudgetForecast {
    let trend = Trend::fit(series);
    let eta_days = match rule.window {
        LimitWindow::Day => trend.days_until_daily(rule.threshold, horizon),
        LimitWindow::Month => trend.days_until_cumulative(rule.threshold - current, horizon),
        LimitWindow::Hour => None,
    }
    // Drop eta==0 (already breached) — only surface a genuinely *pre-emptive* ETA.
    .filter(|&d| d > 0.0);
    BudgetForecast {
        rule_id: rule.id.clone(),
        metric: rule.metric,
        window: rule.window,
        threshold: rule.threshold,
        current: round(current),
        projected_daily: round(trend.project(1.0)),
        trend,
        eta_days,
    }
}

/// Forward look for one customer/product's margin. Revenue is treated as flat over the window
/// (subscriptions are recurring and steady), so the erosion story is driven by the **cost** trend:
/// the dimension turns unprofitable when projected daily cost overtakes the (flat) daily revenue.
#[derive(Debug, Clone, Serialize)]
pub struct MarginForecast {
    pub key: String,
    pub revenue_usd: f64,
    pub cost_usd: f64,
    pub margin_usd: f64,
    /// Recognized revenue spread evenly across the lookback window.
    pub revenue_per_day: f64,
    /// Projected next-day cost.
    pub cost_per_day: f64,
    pub cost_trend: Trend,
    pub currently_profitable: bool,
    /// For a still-profitable dimension: days until projected daily cost overtakes daily revenue —
    /// i.e. when it starts bleeding day-to-day (cumulative margin then erodes). `Some(0.0)` means the
    /// daily crossover is already happening (imminent flip). `None` when the trend never crosses
    /// within the horizon, or the dimension is *already* cumulatively unprofitable (a present fact
    /// reported via `currently_profitable`, not a forecast).
    pub eta_unprofitable_days: Option<f64>,
}

/// Forecast one dimension's margin from its recognized revenue, attributed cost, and daily cost
/// series over the `lookback_days` window.
pub fn forecast_margin(
    key: &str,
    revenue_usd: f64,
    cost_usd: f64,
    cost_series: &[f64],
    lookback_days: u32,
    horizon: u32,
) -> MarginForecast {
    let margin_usd = revenue_usd - cost_usd;
    let revenue_per_day = if lookback_days > 0 {
        revenue_usd / lookback_days as f64
    } else {
        0.0
    };
    let trend = Trend::fit(cost_series);
    // Only forecast a flip for a customer that is still profitable; an already-unprofitable one is a
    // present fact. `Some(0.0)` (daily cost already over daily revenue) is a real signal here — the
    // customer is still net-positive but has begun bleeding day-to-day — so it is *not* dropped.
    let eta_unprofitable_days = if margin_usd > 0.0 && revenue_per_day > 0.0 {
        trend.days_until_daily(revenue_per_day, horizon)
    } else {
        None
    };
    MarginForecast {
        key: key.to_string(),
        revenue_usd: round(revenue_usd),
        cost_usd: round(cost_usd),
        margin_usd: round(margin_usd),
        revenue_per_day: round(revenue_per_day),
        cost_per_day: round(trend.project(1.0)),
        cost_trend: trend,
        currently_profitable: margin_usd > 0.0,
        eta_unprofitable_days,
    }
}

/// Least-squares slope of `y` against `x = 0,1,…,n-1`. `0.0` for fewer than two points or a
/// degenerate (zero-variance) x.
fn least_squares_slope(series: &[f64]) -> f64 {
    let n = series.len();
    if n < 2 {
        return 0.0;
    }
    let nf = n as f64;
    let mean_x = (nf - 1.0) / 2.0;
    let mean_y = series.iter().sum::<f64>() / nf;
    let (mut num, mut den) = (0.0, 0.0);
    for (i, &y) in series.iter().enumerate() {
        let dx = i as f64 - mean_x;
        num += dx * (y - mean_y);
        den += dx * dx;
    }
    if den == 0.0 {
        0.0
    } else {
        num / den
    }
}

fn round(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{LimitAction, LimitMetric, LimitWindow};

    fn cost_rule(window: LimitWindow, threshold: f64) -> LimitRule {
        LimitRule {
            id: "r1".into(),
            project_id: "p1".into(),
            metric: LimitMetric::CostUsd,
            window,
            threshold,
            action: LimitAction::Alert,
            enabled: true,
            warn_at: None,
        }
    }

    #[test]
    fn flat_series_has_zero_slope_and_level_at_value() {
        let t = Trend::fit(&[5.0, 5.0, 5.0, 5.0]);
        assert!(t.slope.abs() < 1e-9);
        assert!((t.level - 5.0).abs() < 1e-9);
        assert!((t.project(10.0) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn rising_series_has_positive_slope() {
        // y = 1,2,3,4,5 → slope 1/day.
        let t = Trend::fit(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert!((t.slope - 1.0).abs() < 1e-9);
        // Projection keeps climbing.
        assert!(t.project(3.0) > t.project(1.0));
    }

    #[test]
    fn projection_floors_at_zero() {
        // Steeply falling: level small, negative slope → projected daily can't go below 0.
        let t = Trend::fit(&[10.0, 6.0, 2.0]);
        assert!(t.slope < 0.0);
        assert_eq!(t.project(100.0), 0.0);
    }

    #[test]
    fn days_until_daily_finds_the_crossing() {
        // ~ +1/day climbing toward a threshold above the current level.
        let t = Trend::fit(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        let eta = t.days_until_daily(10.0, 30).unwrap();
        assert!(eta > 0.0 && eta <= 30.0);
        // A flat trend never reaches a higher threshold.
        assert!(Trend::fit(&[2.0, 2.0, 2.0])
            .days_until_daily(10.0, 30)
            .is_none());
        // Already over → 0 days.
        assert_eq!(Trend::fit(&[9.0, 9.0]).days_until_daily(5.0, 30), Some(0.0));
    }

    #[test]
    fn days_until_cumulative_accumulates_to_budget() {
        // Steady ~$5/day, $12 headroom → crosses partway through day 3.
        let t = Trend::fit(&[5.0, 5.0, 5.0, 5.0]);
        let d = t.days_until_cumulative(12.0, 30).unwrap();
        assert!((d - 2.4).abs() < 0.2, "got {d}");
        // Non-positive headroom → already exhausted.
        assert_eq!(t.days_until_cumulative(0.0, 30), Some(0.0));
        // Tiny daily run-rate never reaches a big budget within the horizon.
        assert!(Trend::fit(&[0.01, 0.01])
            .days_until_cumulative(1000.0, 10)
            .is_none());
    }

    #[test]
    fn daily_budget_breach_is_forecast() {
        // Daily spend climbing 1→5; a $/day cap of 10 is reached in the future, not yet.
        let f = forecast_budget(
            &cost_rule(LimitWindow::Day, 10.0),
            &[1.0, 2.0, 3.0, 4.0, 5.0],
            5.0,
            30,
        );
        let eta = f
            .eta_days
            .expect("a rising daily trend should forecast a breach");
        assert!(eta > 0.0);
        assert!(f.projected_daily > 5.0);
    }

    #[test]
    fn monthly_budget_uses_remaining_headroom() {
        // $200/mo cap, $150 already spent this window, ~$20/day run-rate → ~2.5 days of headroom.
        let f = forecast_budget(
            &cost_rule(LimitWindow::Month, 200.0),
            &[20.0, 20.0, 20.0, 20.0],
            150.0,
            30,
        );
        let eta = f
            .eta_days
            .expect("headroom should run out within the horizon");
        assert!((eta - 2.5).abs() < 0.6, "got {eta}");
    }

    #[test]
    fn hour_window_is_not_forecast() {
        let f = forecast_budget(
            &cost_rule(LimitWindow::Hour, 1.0),
            &[5.0, 6.0, 7.0],
            7.0,
            30,
        );
        assert!(f.eta_days.is_none());
    }

    #[test]
    fn healthy_customer_with_rising_cost_turns_unprofitable() {
        // $60 revenue over 30 days = $2/day. Cost climbing (~+$0.2/day) toward, but still under, $2.
        let f = forecast_margin("acme", 60.0, 20.0, &[0.5, 0.7, 0.9, 1.1, 1.3], 30, 30);
        assert!(f.currently_profitable);
        let eta = f
            .eta_unprofitable_days
            .expect("rising cost should cross the revenue line");
        assert!(eta > 0.0 && eta <= 30.0, "got {eta}");
    }

    #[test]
    fn imminent_daily_crossover_reports_zero_for_profitable_customer() {
        // $30/30d = $1/day. Latest daily cost has just pushed the EWMA level past $1/day, yet the
        // customer is still cumulatively profitable → an imminent (0-day) flip, not "no forecast".
        let f = forecast_margin("acme", 30.0, 12.0, &[0.4, 0.6, 0.8, 1.0, 1.2], 30, 30);
        assert!(f.currently_profitable);
        assert_eq!(f.eta_unprofitable_days, Some(0.0));
    }

    #[test]
    fn already_unprofitable_customer_has_no_eta() {
        // Cost dwarfs revenue already → no future crossing to forecast.
        let f = forecast_margin("heavy", 10.0, 140.0, &[40.0, 45.0, 50.0], 30, 30);
        assert!(!f.currently_profitable);
        assert!(f.eta_unprofitable_days.is_none());
    }

    #[test]
    fn stable_profitable_customer_has_no_eta() {
        // Flat low cost well under the revenue line → never crosses.
        let f = forecast_margin("steady", 300.0, 3.0, &[0.1, 0.1, 0.1, 0.1], 30, 30);
        assert!(f.currently_profitable);
        assert!(f.eta_unprofitable_days.is_none());
    }
}
