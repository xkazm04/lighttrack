//! The rule vocabulary — what a limit measures, over which rolling window, at what threshold, with
//! which reaction — and the pure evaluation of one rule against a current value.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::{CostEvidence, LimitScope, LimitStatus};

/// What a limit measures over its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitMetric {
    #[default]
    CostUsd,
    Calls,
    Tokens,
}

/// Rolling window a limit is evaluated over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitWindow {
    Hour,
    #[default]
    Day,
    Month,
}

impl LimitWindow {
    /// Every window, so a wire parser can derive its accepted set from the enum rather than
    /// hand-maintaining a parallel string list that drifts when a variant is added (the same
    /// authority shape as `Status::ALL`).
    pub const ALL: [LimitWindow; 3] = [LimitWindow::Hour, LimitWindow::Day, LimitWindow::Month];

    /// The wire/storage literal (`hour` | `day` | `month`) — what serde writes.
    pub fn as_str(&self) -> &'static str {
        match self {
            LimitWindow::Hour => "hour",
            LimitWindow::Day => "day",
            LimitWindow::Month => "month",
        }
    }

    /// Parse a wire literal back to a [`LimitWindow`], or `None` outside the vocabulary.
    pub fn from_wire(s: &str) -> Option<LimitWindow> {
        LimitWindow::ALL.into_iter().find(|w| w.as_str() == s)
    }

    /// How long a client should wait before retrying an ingest a **hard** cap turned away. Nothing
    /// frees capacity until usage ages out of the rolling window, so polling faster than this is
    /// pure waste; it is deliberately far shorter than the window itself, because usage leaves the
    /// window continuously rather than all at once. Advisory — the server does not enforce it.
    pub fn retry_after_secs(&self) -> u64 {
        match self {
            LimitWindow::Hour => 30,
            LimitWindow::Day => 300,
            LimitWindow::Month => 900,
        }
    }

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

/// What happens as a limit is approached and breached. Three genuinely distinct tiers:
///
/// - **`Alert`** — observe-only. Notifies; never rejects anything.
/// - **`Throttle`** — *graduated*. Below [`LimitRule::throttle_start`] nothing happens. Between that
///   ratio and the threshold a proportionally growing share of ingest is shed (HTTP 429 with a short
///   `Retry-After`), so a client feels back-pressure and slows down *before* the wall instead of
///   going from fully accepted to fully rejected between two consecutive events. At and above the
///   threshold it is a hard stop, identical to `Block`.
/// - **`Block`** — an unambiguous hard stop at the threshold, with no shedding beforehand. A strict
///   cap stays strict.
///
/// Both enforcing tiers reject at ingest admission (the event is not recorded). Inline *pre-call*
/// blocking still requires the future gateway/proxy mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitAction {
    #[default]
    Alert,
    Throttle,
    Block,
}

impl LimitAction {
    /// Whether breaching a rule with this action rejects ingest (HTTP 429). `Alert` is
    /// observe-only (notify but never block); `Throttle` and `Block` both enforce, so a
    /// configured cap actually caps.
    pub fn enforces(self) -> bool {
        matches!(self, LimitAction::Throttle | LimitAction::Block)
    }

    /// Whether this action sheds traffic *before* the threshold. Only `Throttle` does — that is what
    /// makes it a different tier from `Block` rather than a synonym for it.
    pub fn sheds(self) -> bool {
        matches!(self, LimitAction::Throttle)
    }
}

/// Ratio at which a `Throttle` rule starts shedding when it sets no [`LimitRule::warn_at`]. Chosen
/// to coincide with the default "you're approaching the cap" intuition: the last fifth of the budget
/// is the ramp.
pub const DEFAULT_THROTTLE_START: f64 = 0.8;

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
    /// Optional soft-warning tier: a fraction of the threshold in `(0, 1)`. When rolling usage
    /// reaches `ratio >= warn_at` *without* breaching, a distinct `limit_warning` alert fires (its
    /// own cooldown) so the operator hears about an approaching cap before the 429. `None` = no
    /// pre-warning (old rules deserialize to this, unchanged). Never enforces.
    #[serde(default)]
    pub warn_at: Option<f64>,
    /// Optional dimension this rule caps (provider / model / use-case). `None` (serde-default) =
    /// project-wide, byte-identical to pre-scope behavior. A scoped rule only counts and rejects
    /// traffic matching its scope.
    #[serde(default)]
    pub scope: Option<LimitScope>,
}

fn default_true() -> bool {
    true
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
        if let Some(w) = self.warn_at {
            if !(w.is_finite() && w > 0.0 && w < 1.0) {
                return Err(format!(
                    "warn_at must be a fraction strictly between 0 and 1 (got {w})"
                ));
            }
        }
        Ok(())
    }

    /// The usage ratio at which a `Throttle` rule begins shedding: its [`LimitRule::warn_at`] when
    /// set (the operator already told us where "approaching" starts — reusing it avoids a second
    /// knob that could contradict the first), else [`DEFAULT_THROTTLE_START`]. Meaningless for the
    /// other actions.
    pub fn throttle_start(&self) -> f64 {
        self.warn_at
            .filter(|w| w.is_finite() && *w > 0.0 && *w < 1.0)
            .unwrap_or(DEFAULT_THROTTLE_START)
    }

    /// Pure evaluation: given the project's current value for this rule's metric+window,
    /// decide whether the limit is breached. The caller computes `current` from the store.
    pub fn evaluate(&self, current: f64) -> LimitStatus {
        self.evaluate_with_evidence(current, None)
    }

    /// [`LimitRule::evaluate`] carrying the cost provenance of `current` (see [`CostEvidence`]). The
    /// store passes `Some(..)` for `cost_usd` rules so an operator — and the enforcement decision —
    /// can tell a cap breached on measured spend from one resting on imputation, and so a cap with no
    /// priceable evidence at all rejects instead of reading as a comfortable `$0.00`.
    pub fn evaluate_with_evidence(
        &self,
        current: f64,
        cost_evidence: Option<CostEvidence>,
    ) -> LimitStatus {
        let ratio = if self.threshold > 0.0 {
            current / self.threshold
        } else {
            f64::INFINITY
        };
        let breached = current >= self.threshold;
        // Warning tier: approaching the cap (ratio past warn_at) but not yet breached. A breached
        // rule is never "warning" — it has already crossed into enforcement/breach alerting.
        let warning = !breached && self.warn_at.is_some_and(|w| ratio >= w);
        // Graduated throttling: linear from `throttle_start` (0% shed) to the threshold (100%). At
        // the threshold and beyond the rule is breached and shedding is moot — reported as 1.0 so the
        // signal is continuous rather than snapping back to zero. `Block` and `Alert` never shed.
        let shed_fraction = if !self.action.sheds() {
            0.0
        } else if breached {
            1.0
        } else {
            let start = self.throttle_start();
            ((ratio - start) / (1.0 - start)).clamp(0.0, 1.0)
        };
        LimitStatus {
            rule_id: self.id.clone(),
            project_id: self.project_id.clone(),
            metric: self.metric,
            window: self.window,
            action: self.action,
            current,
            threshold: self.threshold,
            breached,
            ratio,
            warn_at: self.warn_at,
            warning,
            scope: self.scope.clone(),
            cost_evidence,
            shed_fraction,
            shedding: false, // set by the admission path, which knows the candidate event
        }
    }
}
