use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// What a limit measures over its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitMetric {
    CostUsd,
    Calls,
    Tokens,
}

impl Default for LimitMetric {
    fn default() -> Self {
        LimitMetric::CostUsd
    }
}

/// Rolling window a limit is evaluated over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitWindow {
    Hour,
    Day,
    Month,
}

impl Default for LimitWindow {
    fn default() -> Self {
        LimitWindow::Day
    }
}

impl LimitWindow {
    /// Rolling look-back duration for this window (Month is treated as 30 days for now).
    pub fn lookback(&self) -> Duration {
        match self {
            LimitWindow::Hour => Duration::hours(1),
            LimitWindow::Day => Duration::days(1),
            LimitWindow::Month => Duration::days(30),
        }
    }

    /// The start of the rolling window relative to `now`.
    pub fn since(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now - self.lookback()
    }
}

/// What happens when a limit is breached. `Alert` only notifies; `Throttle` and `Block` are
/// **enforced at ingest admission** — a breaching event is rejected with HTTP 429 and not recorded.
/// (Inline *pre-call* blocking still requires the future gateway/proxy mode.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitAction {
    Alert,
    Throttle,
    Block,
}

impl Default for LimitAction {
    fn default() -> Self {
        LimitAction::Alert
    }
}

impl LimitAction {
    /// Whether breaching a rule with this action rejects ingest (HTTP 429). `Alert` is
    /// observe-only (notify but never block); `Throttle` and `Block` both enforce, so a
    /// configured cap actually caps.
    pub fn enforces(self) -> bool {
        matches!(self, LimitAction::Throttle | LimitAction::Block)
    }
}

/// A per-project limit. Tripped by **monitored traffic only** — the scoring engine is exempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitRule {
    pub id: String,
    pub project_id: String,
    pub metric: LimitMetric,
    pub window: LimitWindow,
    pub threshold: f64,
    pub action: LimitAction,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Result of evaluating a rule against a current rolling value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitStatus {
    pub rule_id: String,
    pub project_id: String,
    pub metric: LimitMetric,
    pub window: LimitWindow,
    pub action: LimitAction,
    pub current: f64,
    pub threshold: f64,
    pub breached: bool,
    /// Fraction of the threshold used (1.0 == at limit). Useful for "approaching limit" warnings.
    pub ratio: f64,
}

impl LimitStatus {
    /// True when this breach must reject ingest: a breached rule whose action is enforced
    /// (`Throttle`/`Block`). The ingest path returns HTTP 429 when any status reports this.
    pub fn rejects_ingest(&self) -> bool {
        self.breached && self.action.enforces()
    }
}

impl LimitRule {
    /// Validate a rule's numeric fields before it is created or updated. A `threshold` of `0`,
    /// negative, or non-finite (`NaN`/`inf`) is nonsensical — the old code silently accepted it and
    /// evaluated `ratio = ∞`, so the cap breached on *any* usage. Callers surface the `Err` as HTTP
    /// 400. Kept pure (and here, beside the type) so create and update share exactly one rule.
    pub fn validate(&self) -> Result<(), String> {
        if !(self.threshold.is_finite() && self.threshold > 0.0) {
            return Err(format!(
                "threshold must be a finite number greater than 0 (got {})",
                self.threshold
            ));
        }
        Ok(())
    }

    /// Pure evaluation: given the project's current value for this rule's metric+window,
    /// decide whether the limit is breached. The caller computes `current` from the store.
    pub fn evaluate(&self, current: f64) -> LimitStatus {
        let ratio = if self.threshold > 0.0 {
            current / self.threshold
        } else {
            f64::INFINITY
        };
        LimitStatus {
            rule_id: self.id.clone(),
            project_id: self.project_id.clone(),
            metric: self.metric,
            window: self.window,
            action: self.action,
            current,
            threshold: self.threshold,
            breached: current >= self.threshold,
            ratio,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> LimitRule {
        LimitRule {
            id: "r1".into(),
            project_id: "p1".into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Day,
            threshold: 10.0,
            action: LimitAction::Alert,
            enabled: true,
        }
    }

    #[test]
    fn breaches_at_threshold() {
        assert!(rule().evaluate(10.0).breached);
        assert!(rule().evaluate(12.5).breached);
        assert!(!rule().evaluate(9.99).breached);
    }

    #[test]
    fn ratio_tracks_usage() {
        assert!((rule().evaluate(5.0).ratio - 0.5).abs() < 1e-9);
    }

    #[test]
    fn validate_rejects_nonpositive_or_nonfinite_threshold() {
        let mut r = rule();
        assert!(r.validate().is_ok());
        r.threshold = 0.0;
        assert!(r.validate().is_err(), "zero threshold is invalid");
        r.threshold = -1.0;
        assert!(r.validate().is_err(), "negative threshold is invalid");
        r.threshold = f64::INFINITY;
        assert!(r.validate().is_err(), "non-finite threshold is invalid");
        r.threshold = f64::NAN;
        assert!(r.validate().is_err(), "NaN threshold is invalid");
        r.threshold = 0.0001;
        assert!(r.validate().is_ok(), "small positive threshold is valid");
    }

    #[test]
    fn only_throttle_and_block_enforce() {
        assert!(!LimitAction::Alert.enforces());
        assert!(LimitAction::Throttle.enforces());
        assert!(LimitAction::Block.enforces());
    }

    #[test]
    fn rejects_ingest_requires_breach_and_enforcing_action() {
        let mut r = rule();
        // Breached + enforcing -> reject.
        r.action = LimitAction::Block;
        assert!(r.evaluate(10.0).rejects_ingest());
        r.action = LimitAction::Throttle;
        assert!(r.evaluate(10.0).rejects_ingest());
        // Breached but only Alert -> never rejects.
        r.action = LimitAction::Alert;
        assert!(!r.evaluate(10.0).rejects_ingest());
        // Not breached -> never rejects, even for Block.
        r.action = LimitAction::Block;
        assert!(!r.evaluate(9.99).rejects_ingest());
    }
}
