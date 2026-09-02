//! Margin guardrails: standing policy that turns "this customer is losing us money" into a limit
//! rule, without a human in the loop.
//!
//! The product could already *measure* the loss (`/v1/margin?below=`), *predict* it
//! (`margin_erosion` forecasts with an ETA), and *cap* it (a customer-scoped [`LimitRule`]). Nothing
//! joined the three: the cap's number was hand-typed and went stale on the next invoice. A
//! [`MarginPolicy`] is the join — a trigger, a floor on how much cost is worth acting on, and the
//! action to take — and [`evaluate_policies`] is the pure function that turns today's margin picture
//! into the set of rule changes that would make the policy true.
//!
//! Three properties this module exists to guarantee, all unit-tested below:
//!
//! 1. **Idempotent.** Running it twice on unchanged inputs produces no changes the second time. A
//!    sweep on a timer that churned the rule table every tick would be worse than no sweep.
//! 2. **Origin-scoped.** It only ever creates, updates or deletes rules whose
//!    [`LimitRule::origin`] names the policy that owns them. An operator's hand-made cap is
//!    untouchable by automation, full stop.
//! 3. **Self-expiring.** Every rule it creates carries an `expires_at`, so a guardrail cannot
//!    outlive the sweep that raised it.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::forecast::MarginForecast;
use crate::limits::{
    LimitAction, LimitMetric, LimitRule, LimitScope, LimitWindow, Threshold, ThresholdDimension,
};
use crate::margin::MarginRow;
use crate::new_id;

/// The prefix every policy-created rule's [`LimitRule::origin`] carries.
pub const POLICY_ORIGIN_PREFIX: &str = "margin_policy:";

/// What makes a policy fire for one customer.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyTrigger {
    /// Gross margin percentage under this value (e.g. `20` → margin% < 20%). A cost-only row (no
    /// revenue, so no margin%) qualifies when it is losing money — the free-tier sink is exactly the
    /// case worth catching.
    BelowPct(f64),
    /// Margin is already negative.
    NegativeMargin,
    /// Not yet unprofitable, but forecast to turn unprofitable within this many days.
    ErosionEtaDays(f64),
}

/// What a fired policy does about it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    /// Raise an observe-only rule. The default stance: a guardrail an operator has not yet trusted
    /// should tell them what it *would* have done.
    Warn,
    /// Cap spend at `factor` times the customer's recognized revenue — a
    /// [`Threshold::RevenueShare`] rule, so the cap re-derives itself every invoice instead of going
    /// stale.
    CapToRevenue { factor: f64 },
    /// Throttle at the customer's current cost (graduated shedding up to it).
    Throttle,
    /// Hard-stop at the customer's current cost.
    Block,
}

impl PolicyAction {
    fn limit_action(self) -> LimitAction {
        match self {
            PolicyAction::Warn => LimitAction::Alert,
            PolicyAction::CapToRevenue { .. } => LimitAction::Throttle,
            PolicyAction::Throttle => LimitAction::Throttle,
            PolicyAction::Block => LimitAction::Block,
        }
    }
}

/// A standing margin guardrail for one project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarginPolicy {
    pub id: String,
    pub project_id: String,
    pub trigger: PolicyTrigger,
    /// Floor on the subject's windowed LLM cost before the policy acts. Without it a customer who
    /// cost us four cents and paid nothing would get a guardrail — noise that trains operators to
    /// ignore the feature.
    #[serde(default)]
    pub min_cost_usd: f64,
    pub action: PolicyAction,
    /// Minimum seconds between two applications for the same subject. Enforced by the caller (the
    /// sweep), which is the only thing that knows when it last acted.
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
    /// How long a rule this policy creates stays in force before expiring on its own.
    #[serde(default = "default_expiry")]
    pub expiry_secs: u64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_cooldown() -> u64 {
    3600
}

fn default_expiry() -> u64 {
    86_400
}

fn default_true() -> bool {
    true
}

impl MarginPolicy {
    pub fn validate(&self) -> Result<(), String> {
        match self.trigger {
            PolicyTrigger::BelowPct(p) if !p.is_finite() => {
                return Err("trigger.below_pct must be finite".into())
            }
            PolicyTrigger::ErosionEtaDays(d) if !(d.is_finite() && d > 0.0) => {
                return Err(format!(
                    "trigger.erosion_eta_days must be a finite number greater than 0 (got {d})"
                ))
            }
            _ => {}
        }
        if let PolicyAction::CapToRevenue { factor } = self.action {
            if !(factor.is_finite() && factor > 0.0 && factor <= 10.0) {
                return Err(format!(
                    "action.cap_to_revenue.factor must be in (0, 10] (got {factor})"
                ));
            }
        }
        if !(self.min_cost_usd.is_finite() && self.min_cost_usd >= 0.0) {
            return Err(format!(
                "min_cost_usd must be a finite number >= 0 (got {})",
                self.min_cost_usd
            ));
        }
        if self.expiry_secs == 0 {
            return Err("expiry_secs must be greater than 0".into());
        }
        Ok(())
    }

    /// The origin tag rules this policy owns for `subject` carry. Stable across sweeps — that is
    /// what makes re-application an update rather than a duplicate.
    pub fn origin_for(&self, subject: &str) -> String {
        format!("{POLICY_ORIGIN_PREFIX}{}:{subject}", self.id)
    }
}

/// One change the policy engine wants made to the limit table.
#[derive(Debug, Clone, PartialEq)]
pub enum RuleChange {
    Create(Box<LimitRule>),
    Update(Box<LimitRule>),
    /// A policy-owned rule whose condition has cleared (or whose expiry has passed).
    Delete(String),
}

impl RuleChange {
    /// The subject (customer key) this change is about — the key the sweep applies a cooldown on.
    pub fn subject(&self) -> Option<&str> {
        match self {
            RuleChange::Create(r) | RuleChange::Update(r) => r.scope.as_ref().map(|s| s.value()),
            RuleChange::Delete(_) => None,
        }
    }

    /// The origin of the rule this change carries, when it has one.
    pub fn origin(&self) -> Option<&str> {
        match self {
            RuleChange::Create(r) | RuleChange::Update(r) => r.origin.as_deref(),
            RuleChange::Delete(_) => None,
        }
    }
}

/// Turn today's margin picture into the set of rule changes that make `policies` true.
///
/// Pure: no clock of its own, no store, no ids beyond the one minted for a genuinely new rule. Given
/// the same inputs and the same existing rules it produces the same answer — and given its own
/// output already applied, it produces nothing (the idempotence test below).
///
/// `existing` is the project's full rule set. Only rules whose [`LimitRule::origin`] matches a
/// policy's [`MarginPolicy::origin_for`] are ever considered for update or deletion; every other
/// rule in that slice is read-only to this function.
pub fn evaluate_policies(
    policies: &[MarginPolicy],
    rows: &[MarginRow],
    forecasts: &[MarginForecast],
    existing: &[LimitRule],
    now: DateTime<Utc>,
) -> Vec<RuleChange> {
    let mut out = Vec::new();
    let mut wanted: Vec<String> = Vec::new();

    for p in policies.iter().filter(|p| p.enabled) {
        for row in rows {
            if row.llm_cost_usd < p.min_cost_usd || !fires(p, row, forecasts) {
                continue;
            }
            let origin = p.origin_for(&row.key);
            wanted.push(origin.clone());
            let desired = desired_rule(p, row, &origin, now);
            match existing
                .iter()
                .find(|r| r.origin.as_deref() == Some(&origin))
            {
                None => out.push(RuleChange::Create(Box::new(desired))),
                Some(current) => {
                    // Keep the identity, replace the shape — and only when the shape actually
                    // differs, so a steady state produces no writes.
                    let mut next = desired;
                    next.id = current.id.clone();
                    // `expires_at` slides forward on every sweep by construction; comparing it would
                    // make every pass a change. The renewal is folded in only when something else
                    // differs, or when the rule is within an expiry-window of lapsing.
                    if differs(current, &next) || renewal_due(current, &next, now) {
                        out.push(RuleChange::Update(Box::new(next)));
                    }
                }
            }
        }
    }

    // Reverse pass: a rule this engine owns whose condition has cleared, or which has expired,
    // is removed. Rules owned by nobody (or by a policy that no longer exists in `policies`, which
    // is the same thing from here) are never touched.
    let owned: Vec<&str> = policies.iter().map(|p| p.id.as_str()).collect();
    for r in existing {
        let Some(origin) = r.origin.as_deref() else {
            continue;
        };
        let Some(rest) = origin.strip_prefix(POLICY_ORIGIN_PREFIX) else {
            continue;
        };
        let Some((policy_id, _subject)) = rest.split_once(':') else {
            continue;
        };
        if !owned.contains(&policy_id) {
            continue; // not ours to reap — a policy we were not asked about
        }
        if !wanted.iter().any(|w| w == origin) || r.expires_at.is_some_and(|e| e <= now) {
            out.push(RuleChange::Delete(r.id.clone()));
        }
    }
    out
}

/// Whether `p` fires for `row` given the forecast set.
fn fires(p: &MarginPolicy, row: &MarginRow, forecasts: &[MarginForecast]) -> bool {
    match p.trigger {
        PolicyTrigger::BelowPct(pct) => match row.margin_pct {
            Some(m) => m < pct / 100.0,
            None => row.gross_margin_usd < 0.0,
        },
        PolicyTrigger::NegativeMargin => row.gross_margin_usd < 0.0,
        PolicyTrigger::ErosionEtaDays(days) => forecasts.iter().any(|f| {
            f.key == row.key
                && f.currently_profitable
                && f.eta_unprofitable_days.is_some_and(|e| e <= days)
        }),
    }
}

/// The rule a fired policy wants to exist for `row`.
fn desired_rule(p: &MarginPolicy, row: &MarginRow, origin: &str, now: DateTime<Utc>) -> LimitRule {
    let threshold = match p.action {
        PolicyAction::CapToRevenue { factor } => Threshold::RevenueShare {
            pct: factor * 100.0,
            dimension: ThresholdDimension::Customer,
        },
        // Warn / Throttle / Block cap at what the subject is spending today, so the guardrail bites
        // at the current burn rather than at a number nobody chose. Never zero or negative: a $0
        // threshold breaches on any traffic at all, which is a block by accident.
        _ => Threshold::Fixed(row.llm_cost_usd.max(0.01)),
    };
    LimitRule {
        id: new_id(),
        project_id: String::new(), // filled by the caller, which knows the project
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Month,
        threshold,
        action: p.action.limit_action(),
        enabled: true,
        warn_at: None,
        scope: Some(LimitScope::Customer(row.key.clone())),
        escalation: None,
        escalated_until: None,
        origin: Some(origin.to_string()),
        expires_at: Some(now + Duration::seconds(p.expiry_secs as i64)),
    }
}

/// Whether two rules differ in anything the policy engine controls (identity and expiry aside).
fn differs(a: &LimitRule, b: &LimitRule) -> bool {
    a.metric != b.metric
        || a.window != b.window
        || a.threshold != b.threshold
        || a.action != b.action
        || a.enabled != b.enabled
        || a.scope != b.scope
}

/// Is the existing rule close enough to lapsing that this sweep should renew it? Renewing only in
/// the last third of its life keeps a steady state write-free while guaranteeing an active policy's
/// rule never actually expires under it.
fn renewal_due(current: &LimitRule, next: &LimitRule, now: DateTime<Utc>) -> bool {
    match (current.expires_at, next.expires_at) {
        (Some(cur), Some(want)) => {
            let full = (want - now).num_seconds().max(1);
            (cur - now).num_seconds() * 3 <= full
        }
        (None, Some(_)) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forecast::Trend;

    /// A flat, uninteresting cost trend — the erosion tests care about `eta_unprofitable_days`,
    /// which the forecaster has already computed by the time a policy sees it.
    fn trend() -> Trend {
        Trend {
            level: 0.0,
            slope: 0.0,
            n: 0,
        }
    }

    fn row(key: &str, revenue: f64, cost: f64) -> MarginRow {
        let margin = revenue - cost;
        MarginRow {
            key: key.into(),
            revenue_usd: revenue,
            llm_cost_usd: cost,
            gross_margin_usd: margin,
            margin_pct: (revenue > 0.0).then(|| margin / revenue),
            calls: 10,
        }
    }

    fn policy(trigger: PolicyTrigger, action: PolicyAction) -> MarginPolicy {
        MarginPolicy {
            id: "pol-1".into(),
            project_id: "proj".into(),
            trigger,
            min_cost_usd: 1.0,
            action,
            cooldown_secs: 3600,
            expiry_secs: 86_400,
            enabled: true,
        }
    }

    /// Property 1: the same inputs plus the engine's own output produce nothing further. A sweep on
    /// a timer that churned the rule table every tick would be worse than no sweep at all.
    #[test]
    fn evaluate_policies_is_idempotent() {
        let now = Utc::now();
        let p = policy(PolicyTrigger::NegativeMargin, PolicyAction::Warn);
        let rows = vec![row("cus-a", 10.0, 40.0)];
        let first = evaluate_policies(std::slice::from_ref(&p), &rows, &[], &[], now);
        assert_eq!(first.len(), 1, "one new guardrail");
        let RuleChange::Create(created) = &first[0] else {
            panic!("expected a create, got {:?}", first[0]);
        };
        let applied = vec![(**created).clone()];
        let second = evaluate_policies(&[p], &rows, &[], &applied, now);
        assert!(
            second.is_empty(),
            "a second pass over unchanged inputs must change nothing, got {second:?}"
        );
    }

    /// Property 2: automation never edits a rule it did not create.
    #[test]
    fn a_rule_without_a_matching_origin_is_never_touched() {
        let now = Utc::now();
        let p = policy(PolicyTrigger::NegativeMargin, PolicyAction::Block);
        let rows = vec![row("cus-a", 0.0, 40.0)];
        let hand_made = LimitRule {
            id: "human-rule".into(),
            project_id: "proj".into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Month,
            threshold: Threshold::Fixed(5.0),
            action: LimitAction::Block,
            enabled: true,
            warn_at: None,
            scope: Some(LimitScope::Customer("cus-a".into())),
            escalation: None,
            escalated_until: None,
            origin: None,
            expires_at: None,
        };
        // Also a rule owned by a *different* policy, which this call was not asked about.
        let other_policys = LimitRule {
            id: "other-rule".into(),
            origin: Some("margin_policy:pol-2:cus-a".into()),
            ..hand_made.clone()
        };
        let changes = evaluate_policies(
            &[p],
            &rows,
            &[],
            &[hand_made.clone(), other_policys.clone()],
            now,
        );
        for c in &changes {
            match c {
                RuleChange::Delete(id) => panic!("deleted a rule it does not own: {id}"),
                RuleChange::Update(r) => {
                    panic!("updated a rule it does not own: {}", r.id)
                }
                RuleChange::Create(_) => {}
            }
        }
        assert_eq!(changes.len(), 1, "only its own new rule: {changes:?}");
    }

    /// The reverse pass: once the customer is profitable again, the guardrail this engine raised is
    /// taken back down.
    #[test]
    fn a_cleared_condition_removes_the_policys_own_rule() {
        let now = Utc::now();
        let p = policy(PolicyTrigger::NegativeMargin, PolicyAction::Warn);
        let losing = vec![row("cus-a", 10.0, 40.0)];
        let first = evaluate_policies(std::slice::from_ref(&p), &losing, &[], &[], now);
        let RuleChange::Create(created) = &first[0] else {
            panic!("expected create");
        };
        let applied = vec![(**created).clone()];
        let healthy = vec![row("cus-a", 100.0, 40.0)];
        let changes = evaluate_policies(&[p], &healthy, &[], &applied, now);
        assert_eq!(
            changes,
            vec![RuleChange::Delete(created.id.clone())],
            "the guardrail comes back down when the loss does"
        );
    }

    #[test]
    fn cap_to_revenue_creates_a_self_rederiving_revenue_share_rule() {
        let now = Utc::now();
        let p = policy(
            PolicyTrigger::BelowPct(20.0),
            PolicyAction::CapToRevenue { factor: 0.8 },
        );
        let rows = vec![row("cus-a", 100.0, 90.0)]; // 10% margin
        let changes = evaluate_policies(&[p], &rows, &[], &[], now);
        let RuleChange::Create(r) = &changes[0] else {
            panic!("expected create");
        };
        assert_eq!(
            r.threshold,
            Threshold::RevenueShare {
                pct: 80.0,
                dimension: ThresholdDimension::Customer
            }
        );
        assert_eq!(r.scope, Some(LimitScope::Customer("cus-a".into())));
        assert!(r
            .origin
            .as_deref()
            .unwrap()
            .starts_with(POLICY_ORIGIN_PREFIX));
        assert!(r.expires_at.is_some(), "a policy rule always self-expires");
    }

    #[test]
    fn the_min_cost_floor_keeps_trivial_losses_quiet() {
        let now = Utc::now();
        let mut p = policy(PolicyTrigger::NegativeMargin, PolicyAction::Warn);
        p.min_cost_usd = 25.0;
        let rows = vec![row("cus-tiny", 0.0, 0.04)];
        assert!(evaluate_policies(&[p], &rows, &[], &[], now).is_empty());
    }

    #[test]
    fn erosion_fires_only_for_a_forecast_inside_the_eta() {
        let now = Utc::now();
        let p = policy(PolicyTrigger::ErosionEtaDays(5.0), PolicyAction::Warn);
        let rows = vec![row("cus-a", 100.0, 40.0)]; // profitable today
        let mk = |eta: Option<f64>| MarginForecast {
            key: "cus-a".into(),
            revenue_usd: 100.0,
            cost_usd: 40.0,
            margin_usd: 60.0,
            revenue_per_day: 3.0,
            cost_per_day: 2.0,
            cost_trend: trend(),
            currently_profitable: true,
            eta_unprofitable_days: eta,
        };
        assert!(
            evaluate_policies(std::slice::from_ref(&p), &rows, &[mk(Some(3.0))], &[], now).len()
                == 1
        );
        assert!(
            evaluate_policies(std::slice::from_ref(&p), &rows, &[mk(Some(9.0))], &[], now)
                .is_empty()
        );
        assert!(evaluate_policies(&[p], &rows, &[mk(None)], &[], now).is_empty());
    }

    #[test]
    fn an_expired_policy_rule_is_reaped_even_while_the_condition_holds() {
        let now = Utc::now();
        let p = policy(PolicyTrigger::NegativeMargin, PolicyAction::Warn);
        let rows = vec![row("cus-a", 0.0, 40.0)];
        let stale = LimitRule {
            id: "stale".into(),
            project_id: "proj".into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Month,
            threshold: Threshold::Fixed(40.0),
            action: LimitAction::Alert,
            enabled: true,
            warn_at: None,
            scope: Some(LimitScope::Customer("cus-a".into())),
            escalation: None,
            escalated_until: None,
            origin: Some(p.origin_for("cus-a")),
            expires_at: Some(now - Duration::hours(1)),
        };
        let changes = evaluate_policies(&[p], &rows, &[], &[stale], now);
        assert!(
            changes.contains(&RuleChange::Delete("stale".into())),
            "an expired guardrail is removed: {changes:?}"
        );
    }
}
