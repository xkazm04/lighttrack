//! The act half of measure→act: the two passes the forecast sweep runs after it has computed a
//! forecast, and the only place in the system that writes a limit rule without a human.
//!
//! **Escalation** (`escalate`). A `budget_breach` forecast already knew a project breaches in ~2
//! days; the only thing that ever happened with that knowledge was a Slack message. A rule carrying
//! an [`Escalation`] whose `on_eta_days` the forecast has reached gets `escalated_until` stamped, so
//! it *acts* as the tighter action until it lapses. The configured action is never overwritten, so
//! the reverse pass is a field clear rather than a remembered undo.
//!
//! **Policies** (`apply_policies`). [`evaluate_policies`] is pure and already tested for the two
//! properties that make automation safe (idempotent, and it never touches a rule it does not own).
//! This module is the I/O around it: read the margin picture, hand it over, write back the changes
//! the cooldown lets through.
//!
//! Three rules this module holds itself to, because the failure mode of a rule-writing background
//! task is an operator waking up to caps nobody chose:
//!
//! 1. Every write is preceded by a `tracing::info!` naming the rule, the origin and the reason.
//! 2. A backend that cannot answer part of the picture makes the pass **skip**, not guess — and
//!    warns once per sweep rather than once per project, so a Postgres deployment does not get one
//!    line per project per tick forever.
//! 3. Nothing here runs unless the sweep is on, which is off by default.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{Duration, Utc};

use lighttrack_core::forecast::MarginForecast;
use lighttrack_core::{evaluate_policies, LimitRule, MarginRow, RuleChange};

use crate::forecast_alerts::ForecastAlert;
use crate::state::{spawn_db, AppState};
use lighttrack_store::Scope as TenantScope;

/// Per-(policy, subject) instant of the last applied change, so a policy with a long cooldown does
/// not re-apply on every tick. Process-local and lost on restart — the same stance (and the same
/// consequence: at worst one extra application after a deploy) as the alert cooldown map.
#[derive(Default)]
pub(crate) struct PolicyCooldowns {
    seen: Mutex<HashMap<String, chrono::DateTime<Utc>>>,
}

impl PolicyCooldowns {
    /// Whether `origin` may be acted on now, recording the attempt when it may.
    fn allow(&self, origin: &str, cooldown_secs: u64, now: chrono::DateTime<Utc>) -> bool {
        let Ok(mut seen) = self.seen.lock() else {
            return false; // a poisoned lock must not become "apply everything, always"
        };
        if let Some(last) = seen.get(origin) {
            if now - *last < Duration::seconds(cooldown_secs as i64) {
                return false;
            }
        }
        seen.insert(origin.to_string(), now);
        true
    }
}

/// What one project's guardrail pass did, for the sweep's own log line.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct GuardrailOutcome {
    pub escalated: usize,
    pub de_escalated: usize,
    pub rules_created: usize,
    pub rules_updated: usize,
    pub rules_removed: usize,
}

impl GuardrailOutcome {
    pub(crate) fn total(&self) -> usize {
        self.escalated
            + self.de_escalated
            + self.rules_created
            + self.rules_updated
            + self.rules_removed
    }
}

/// Pass (a): apply and reverse escalations for one project, given the forecast's alerts.
///
/// Pure decision, I/O around it. A rule escalates when a `budget_breach` alert names it and the ETA
/// has reached its `on_eta_days`; it de-escalates when no such alert stands any more. The lapse is
/// also written into the rule (`escalated_until`), so a sweep that stops running cannot leave a
/// project throttled indefinitely.
pub(crate) async fn escalate(
    st: &AppState,
    project: &str,
    alerts: &[ForecastAlert],
) -> Result<GuardrailOutcome, crate::error::ApiError> {
    let store = st.store.clone();
    let pid = project.to_string();
    let rules = spawn_db(move || store.list_limit_rules(&pid, false)).await?;
    let now = Utc::now();
    let mut out = GuardrailOutcome::default();

    for rule in rules.iter().filter(|r| r.escalation.is_some()) {
        let Some(esc) = rule.escalation else { continue };
        let breaching = alerts.iter().any(|a| {
            a.kind == "budget_breach" && a.subject == rule.id && a.eta_days <= esc.on_eta_days
        });
        let live = rule.escalated_until.is_some_and(|u| u > now);
        let next = match (breaching, live) {
            // Already escalated and still breaching: leave it alone. Re-stamping every tick would
            // make the escalation immortal, which is the opposite of a bounded response.
            (true, true) | (false, false) => continue,
            (true, false) => Some(esc.until(now)),
            (false, true) => None,
        };
        let mut updated = rule.clone();
        updated.escalated_until = next;
        tracing::info!(
            project_id = %project,
            rule_id = %rule.id,
            from = ?rule.action,
            to = ?esc.to,
            escalating = next.is_some(),
            "forecast sweep {} a limit rule",
            if next.is_some() { "escalated" } else { "de-escalated" }
        );
        let store = st.store.clone();
        let owner = project.to_string();
        if spawn_db(move || store.update_limit_rule(TenantScope::Project(&owner), &updated)).await?
        {
            if next.is_some() {
                out.escalated += 1;
            } else {
                out.de_escalated += 1;
            }
        }
    }
    Ok(out)
}

/// Pass (b): evaluate this project's margin policies and apply the rule changes they ask for.
///
/// `rows` and `forecasts` are the margin picture the sweep has already computed — passed in rather
/// than re-read, so the guardrail acts on exactly the numbers the alert was raised from.
pub(crate) async fn apply_policies(
    st: &AppState,
    project: &str,
    rows: &[MarginRow],
    forecasts: &[MarginForecast],
) -> Result<GuardrailOutcome, crate::error::ApiError> {
    let store = st.store.clone();
    let pid = project.to_string();
    let policies = spawn_db(move || store.list_margin_policies(&pid, true)).await?;
    if policies.is_empty() {
        return Ok(GuardrailOutcome::default());
    }
    let store = st.store.clone();
    let pid = project.to_string();
    let existing = spawn_db(move || store.list_limit_rules(&pid, false)).await?;

    let now = Utc::now();
    let changes = evaluate_policies(&policies, rows, forecasts, &existing, now);
    let mut out = GuardrailOutcome::default();
    for change in changes {
        // The cooldown is checked here rather than inside the pure engine, because only the process
        // knows when it last acted. A change the cooldown blocks is simply not applied this tick;
        // the engine will propose it again next sweep, unchanged (it is idempotent).
        if let Some(origin) = change.origin() {
            let cooldown = policies
                .iter()
                .find(|p| origin.starts_with(&format!("margin_policy:{}:", p.id)))
                .map_or(0, |p| p.cooldown_secs);
            if !st.policy_cooldowns.allow(origin, cooldown, now) {
                continue;
            }
        }
        apply_one(st, project, change, &mut out).await?;
    }
    Ok(out)
}

async fn apply_one(
    st: &AppState,
    project: &str,
    change: RuleChange,
    out: &mut GuardrailOutcome,
) -> Result<(), crate::error::ApiError> {
    match change {
        RuleChange::Create(mut rule) => {
            rule.project_id = project.to_string();
            tracing::info!(
                project_id = %project,
                rule_id = %rule.id,
                origin = rule.origin.as_deref().unwrap_or(""),
                action = ?rule.action,
                "margin policy created a limit rule"
            );
            let store = st.store.clone();
            let r = (*rule).clone();
            spawn_db(move || store.create_limit_rule(&r)).await?;
            out.rules_created += 1;
        }
        RuleChange::Update(mut rule) => {
            rule.project_id = project.to_string();
            tracing::info!(
                project_id = %project,
                rule_id = %rule.id,
                origin = rule.origin.as_deref().unwrap_or(""),
                "margin policy refreshed its limit rule"
            );
            let store = st.store.clone();
            let r = (*rule).clone();
            let owner = project.to_string();
            spawn_db(move || store.update_limit_rule(TenantScope::Project(&owner), &r)).await?;
            out.rules_updated += 1;
        }
        RuleChange::Delete(id) => {
            tracing::info!(
                project_id = %project,
                rule_id = %id,
                "margin policy withdrew its limit rule (condition cleared or expired)"
            );
            let store = st.store.clone();
            let id2 = id.clone();
            let owner = project.to_string();
            spawn_db(move || store.delete_limit_rule(TenantScope::Project(&owner), &id2)).await?;
            out.rules_removed += 1;
        }
    }
    Ok(())
}

/// Which rule, if any, a policy raised for `key` — what `/v1/margin` shows as a row's `guardrail`.
pub(crate) fn guardrail_for<'a>(rules: &'a [LimitRule], key: &str) -> Option<&'a str> {
    rules
        .iter()
        .find(|r| {
            r.origin
                .as_deref()
                .is_some_and(|o| o.ends_with(&format!(":{key}")))
        })
        .map(|r| r.id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cooldown_blocks_a_repeat_and_releases_after_its_window() {
        let c = PolicyCooldowns::default();
        let t0 = Utc::now();
        assert!(c.allow("margin_policy:p:cus", 60, t0), "first pass applies");
        assert!(
            !c.allow("margin_policy:p:cus", 60, t0 + Duration::seconds(30)),
            "a repeat inside the cooldown is skipped, not applied"
        );
        assert!(
            c.allow("margin_policy:p:cus", 60, t0 + Duration::seconds(61)),
            "past the cooldown it applies again"
        );
        assert!(
            c.allow("margin_policy:p:other", 60, t0),
            "a different subject is independent"
        );
    }

    #[test]
    fn a_zero_cooldown_never_blocks() {
        let c = PolicyCooldowns::default();
        let t0 = Utc::now();
        assert!(c.allow("o", 0, t0));
        assert!(c.allow("o", 0, t0));
    }

    #[test]
    fn guardrail_for_matches_only_the_rule_raised_for_that_key() {
        use lighttrack_core::{LimitAction, LimitMetric, LimitWindow, Threshold};
        let mk = |id: &str, origin: Option<&str>| LimitRule {
            id: id.into(),
            project_id: "p".into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Month,
            threshold: Threshold::Fixed(1.0),
            action: LimitAction::Alert,
            enabled: true,
            warn_at: None,
            scope: None,
            escalation: None,
            escalated_until: None,
            origin: origin.map(str::to_string),
            expires_at: None,
        };
        let rules = vec![
            mk("hand-made", None),
            mk("guard", Some("margin_policy:pol-1:cus-a")),
        ];
        assert_eq!(guardrail_for(&rules, "cus-a"), Some("guard"));
        assert_eq!(guardrail_for(&rules, "cus-b"), None);
    }
}
