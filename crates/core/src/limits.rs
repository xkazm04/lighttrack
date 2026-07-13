use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Optional dimension a limit is scoped to — one of provider / model / use-case (`name`). An
/// unscoped rule (`None` on [`LimitRule::scope`]) applies to the whole project, exactly as before;
/// a scoped rule only counts (and can reject) traffic matching the selected dimension value, so an
/// operator can "cap gpt-4o at $5/day" or "cap use-case X" without touching other traffic.
///
/// Serializes externally-tagged, e.g. `{"model":"gpt-4o"}` / `{"provider":"openai"}` /
/// `{"name":"summarize"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitScope {
    Provider(String),
    Model(String),
    /// Use-case, matched against an event's `name`.
    Name(String),
}

impl LimitScope {
    /// The storage discriminant (`provider` | `model` | `name`).
    pub fn kind_str(&self) -> &'static str {
        match self {
            LimitScope::Provider(_) => "provider",
            LimitScope::Model(_) => "model",
            LimitScope::Name(_) => "name",
        }
    }

    /// The scoped value.
    pub fn value(&self) -> &str {
        match self {
            LimitScope::Provider(v) | LimitScope::Model(v) | LimitScope::Name(v) => v,
        }
    }

    /// Reconstruct from stored `(kind, value)` columns; `None` for an unknown kind.
    pub fn from_parts(kind: &str, value: String) -> Option<LimitScope> {
        match kind {
            "provider" => Some(LimitScope::Provider(value)),
            "model" => Some(LimitScope::Model(value)),
            "name" => Some(LimitScope::Name(value)),
            _ => None,
        }
    }

    /// A compact `kind=value` label for alert messages / dedup keys / rendering.
    pub fn label(&self) -> String {
        format!("{}={}", self.kind_str(), self.value())
    }

    /// Whether an event with these dimensions falls under this scope.
    pub fn matches(&self, provider: &str, model: &str, name: Option<&str>) -> bool {
        match self {
            LimitScope::Provider(v) => provider == v,
            LimitScope::Model(v) => model == v,
            LimitScope::Name(v) => name == Some(v.as_str()),
        }
    }
}

/// Whether a rule's optional scope admits an event with these dimensions. `None` (unscoped) always
/// matches — identical to pre-scope behavior.
pub fn scope_matches(
    scope: Option<&LimitScope>,
    provider: &str,
    model: &str,
    name: Option<&str>,
) -> bool {
    scope.map_or(true, |s| s.matches(provider, model, name))
}

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
    /// The rule's configured soft-warning fraction, echoed for the status surface (`None` = none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_at: Option<f64>,
    /// In the soft-warning tier: at/over `warn_at` but not yet breached. Drives the "warning" badge
    /// on the status surface and the `limit_warning` alert. Always `false` when `warn_at` is unset.
    #[serde(default)]
    pub warning: bool,
    /// The rule's dimension scope, echoed for the status surface / alerts (`None` = project-wide).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<LimitScope>,
}

impl LimitStatus {
    /// True when this breach must reject ingest: a breached rule whose action is enforced
    /// (`Throttle`/`Block`). The ingest path returns HTTP 429 when any status reports this.
    pub fn rejects_ingest(&self) -> bool {
        self.breached && self.action.enforces()
    }

    /// A compact scope tag for keys/labels: `all` when project-wide, else `kind=value`.
    pub fn scope_tag(&self) -> String {
        match &self.scope {
            None => "all".to_string(),
            Some(s) => s.label(),
        }
    }

    /// Stable per-rule key for alert-cooldown dedup and for matching a breach to its running
    /// rejection count. Includes the scope so a scoped cap and a project-wide cap on the same
    /// metric+window don't collide on one key.
    pub fn alert_key(&self) -> String {
        format!("{}:{:?}:{:?}:{}", self.project_id, self.metric, self.window, self.scope_tag())
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
        if let Some(w) = self.warn_at {
            if !(w.is_finite() && w > 0.0 && w < 1.0) {
                return Err(format!(
                    "warn_at must be a fraction strictly between 0 and 1 (got {w})"
                ));
            }
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
        let breached = current >= self.threshold;
        // Warning tier: approaching the cap (ratio past warn_at) but not yet breached. A breached
        // rule is never "warning" — it has already crossed into enforcement/breach alerting.
        let warning = !breached && self.warn_at.is_some_and(|w| ratio >= w);
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
            warn_at: None,
            scope: None,
        }
    }

    #[test]
    fn scope_matches_dimension() {
        let s = LimitScope::Model("gpt-4o".into());
        assert!(s.matches("openai", "gpt-4o", None), "model matches");
        assert!(!s.matches("openai", "gpt-4o-mini", None), "other model does not");
        let p = LimitScope::Provider("openai".into());
        assert!(p.matches("openai", "gpt-4o", Some("x")));
        assert!(!p.matches("anthropic", "claude", None));
        let n = LimitScope::Name("summarize".into());
        assert!(n.matches("openai", "gpt-4o", Some("summarize")));
        assert!(!n.matches("openai", "gpt-4o", None), "unnamed event doesn't match a name scope");
        // Unscoped always matches.
        assert!(scope_matches(None, "any", "any", None));
    }

    #[test]
    fn scope_roundtrips_through_parts_and_key() {
        let s = LimitScope::Model("gpt-4o".into());
        assert_eq!(LimitScope::from_parts(s.kind_str(), s.value().to_string()), Some(s.clone()));
        assert_eq!(s.label(), "model=gpt-4o");
        let mut r = rule();
        r.scope = Some(s);
        let st = r.evaluate(5.0);
        assert_eq!(st.scope_tag(), "model=gpt-4o");
        assert!(st.alert_key().ends_with(":model=gpt-4o"));
        // Unscoped tag/key.
        assert_eq!(rule().evaluate(5.0).scope_tag(), "all");
    }

    #[test]
    fn warn_at_sets_warning_below_breach() {
        let mut r = rule();
        r.warn_at = Some(0.8);
        // Below warn_at: neither warning nor breached.
        let s = r.evaluate(7.0);
        assert!(!s.warning && !s.breached);
        // At/over warn_at, under threshold: warning, not breached.
        let s = r.evaluate(8.5);
        assert!(s.warning && !s.breached, "crossing warn_at warns without breaching");
        // At threshold: breached, and warning is suppressed (already past the cap).
        let s = r.evaluate(10.0);
        assert!(s.breached && !s.warning);
    }

    #[test]
    fn validate_rejects_bad_warn_at() {
        let mut r = rule();
        r.warn_at = Some(1.0);
        assert!(r.validate().is_err(), "warn_at must be < 1");
        r.warn_at = Some(0.0);
        assert!(r.validate().is_err(), "warn_at must be > 0");
        r.warn_at = Some(f64::NAN);
        assert!(r.validate().is_err());
        r.warn_at = Some(0.8);
        assert!(r.validate().is_ok());
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
