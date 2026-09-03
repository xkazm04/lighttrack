//! Turning a forecast into pre-emptive warnings: the `ForecastAlert` type, its dedup key, and the
//! pure `budgets + margins -> alerts` mapping.
//!
//! Split out of [`super::forecast`] so the wiring module stays under the file-size bar and so the
//! scheduled sweep ([`crate::forecast_sweep`]) and the `GET /v1/forecast` handler provably build
//! their alerts from one function rather than two that could drift.

use serde::Serialize;

use lighttrack_core::forecast::{BudgetForecast, MarginForecast, Trend};
use lighttrack_core::{LimitMetric, LimitWindow};

/// A pre-emptive forecast warning. `severity` is `high` when the event is <=3 days out, else `warning`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ForecastAlert {
    pub kind: &'static str, // "budget_breach" | "margin_erosion"
    pub severity: &'static str,
    pub project_id: String,
    /// The rule id (budget) or customer/product key (margin) the alert is about.
    pub subject: String,
    pub eta_days: f64,
    pub message: String,
    /// For a `margin_erosion` alert: the id of the limit rule a margin policy has standing for this
    /// subject, when one exists. It turns the alert from "someone should do something" into "this
    /// is what is already being done", which is the difference between a warning and a report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_applied: Option<String>,
}

impl ForecastAlert {
    /// Stable dedup key so a sustained forecast doesn't re-alert every poll (cooldown in the sink).
    ///
    /// Deliberately carries no trace of *how* the forecast was produced: a scheduled sweep and a
    /// hand-made `GET /v1/forecast` for the same project share this key, so turning the sweep on
    /// cannot double the volume an operator receives.
    /// Carries `severity` because an escalation is news. The same subject going from `warning` to
    /// `high` inside one cooldown window is precisely the message worth sending — under a
    /// severity-free key it was the one message the cooldown swallowed.
    pub(crate) fn dedup_key(&self) -> String {
        format!(
            "forecast:{}:{}:{}:{}",
            self.project_id, self.kind, self.subject, self.severity
        )
    }
}

/// May this trend page anyone? Two independent conditions, both required:
///
/// * it clears the **evidence floor** — enough non-zero days, spanning enough of them, that the fit
///   is describing the project rather than the window's zero-fill; and
/// * the **burn rate corroborates** it — the last few days really are running above the window's
///   own baseline, so the ETA is carried by what is happening now and not by an old spike still
///   inside the lookback.
///
/// Applied here, in the one function both the handler and the scheduled sweep call, so neither can
/// deliver an alert the other would have withheld.
fn may_page(t: &Trend) -> bool {
    t.is_presentable() && t.corroborated()
}

/// "(confidence r²=0.87 over 12 days)", or nothing at all when the fit is under the floor — an
/// alert never quotes a confidence the forecast surface refused to publish.
fn confidence_note(t: &Trend) -> String {
    match t.r2 {
        Some(r2) => format!(" (confidence r²={r2:.2} over {} days)", t.span_days),
        None => String::new(),
    }
}

pub(crate) fn build_alerts(
    project: &str,
    budgets: &[BudgetForecast],
    margins: &[MarginForecast],
) -> Vec<ForecastAlert> {
    let mut out = Vec::new();
    for b in budgets.iter().filter(|b| may_page(&b.trend)) {
        if let Some(eta) = b.eta_days {
            out.push(ForecastAlert {
                kind: "budget_breach",
                severity: severity(eta),
                project_id: project.to_string(),
                subject: b.rule_id.clone(),
                eta_days: round2(eta),
                policy_applied: None,
                message: format!(
                    "project '{project}' is on track to breach its {} {} budget ({:.4}) {} — \
                     projected ~{:.4}/day, current rolling {:.4}{}",
                    window_word(b.window),
                    metric_word(b.metric),
                    b.threshold,
                    humanize(eta),
                    b.projected_daily,
                    b.current,
                    confidence_note(&b.trend),
                ),
            });
        }
    }
    for m in margins.iter().filter(|m| may_page(&m.cost_trend)) {
        if m.currently_profitable {
            if let Some(eta) = m.eta_unprofitable_days {
                out.push(ForecastAlert {
                    kind: "margin_erosion",
                    severity: severity(eta),
                    project_id: project.to_string(),
                    subject: m.key.clone(),
                    eta_days: round2(eta),
                    policy_applied: None,
                    message: format!(
                        "'{}' is on track to turn unprofitable {} — revenue ~${:.2}/day vs cost \
                         rising to ~${:.2}/day{}",
                        m.key,
                        humanize(eta),
                        m.revenue_per_day,
                        m.cost_per_day,
                        confidence_note(&m.cost_trend),
                    ),
                });
            }
        } else if m.cost_trend.effective_slope() > 0.0 {
            out.push(ForecastAlert {
                kind: "margin_erosion",
                severity: "high",
                project_id: project.to_string(),
                subject: m.key.clone(),
                eta_days: 0.0,
                policy_applied: None,
                message: format!(
                    "'{}' is already unprofitable (margin ${:.2}) and cost is still rising{}",
                    m.key,
                    m.margin_usd,
                    confidence_note(&m.cost_trend),
                ),
            });
        }
    }
    out
}

/// Stamp `policy_applied` on every `margin_erosion` alert whose subject has a policy-raised
/// guardrail standing. Applied after [`build_alerts`] because only the caller has read the rules;
/// keeping the mapping pure is what lets it be tested without a store.
///
/// Only an **enabled** rule stands: the sweep reads every rule so the stamp sees ones it just
/// raised, and a guardrail an operator switched off is not "what is already being done".
pub(crate) fn attach_guardrails(
    alerts: &mut [ForecastAlert],
    rules: &[lighttrack_core::LimitRule],
) {
    let standing: Vec<lighttrack_core::LimitRule> =
        rules.iter().filter(|r| r.enabled).cloned().collect();
    for a in alerts.iter_mut().filter(|a| a.kind == "margin_erosion") {
        a.policy_applied =
            crate::margin_guardrails::guardrail_for(&standing, &a.subject).map(str::to_string);
    }
}

fn severity(eta_days: f64) -> &'static str {
    if eta_days <= 3.0 {
        "high"
    } else {
        "warning"
    }
}

/// Human phrasing for an ETA, matching the "about 3 days" / "next week" feel of the headline alerts.
fn humanize(eta_days: f64) -> String {
    if eta_days < 1.0 {
        "imminently".to_string()
    } else if eta_days < 14.0 {
        format!("in about {eta_days:.0} days")
    } else {
        format!("in about {:.0} weeks", eta_days / 7.0)
    }
}

fn metric_word(m: LimitMetric) -> &'static str {
    match m {
        LimitMetric::CostUsd => "cost",
        LimitMetric::Calls => "calls",
        LimitMetric::Tokens => "tokens",
    }
}

fn window_word(w: LimitWindow) -> &'static str {
    match w {
        LimitWindow::Hour => "hourly",
        LimitWindow::Day => "daily",
        LimitWindow::Month => "monthly",
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_core::forecast::forecast_budget;
    use lighttrack_core::{LimitAction, LimitRule, Threshold};

    fn rule() -> LimitRule {
        LimitRule {
            id: "r1".into(),
            project_id: "p1".into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Day,
            threshold: Threshold::Fixed(50.0),
            action: LimitAction::Alert,
            enabled: true,
            warn_at: None,
            scope: None,
            escalation: None,
            escalated_until: None,
            origin: None,
            expires_at: None,
        }
    }

    #[test]
    fn a_thin_series_pages_nobody_however_steep_it_looks() {
        // Twelve zero-filled days then two real ones — a slope by construction, evidence of nothing.
        let mut series = vec![0.0; 12];
        series.extend([2.0, 20.0]);
        let b = forecast_budget(&rule(), &series, 20.0, 90);
        assert!(
            b.eta_days.is_some(),
            "the arithmetic still finds a crossing"
        );
        assert!(
            build_alerts("p1", &[b], &[]).is_empty(),
            "the gate is what stands between that crossing and an operator's phone"
        );
    }

    #[test]
    fn an_old_spike_that_has_since_cooled_is_not_corroborated() {
        // Six real days, so the evidence floor is cleared — but the burn rate is falling away.
        let b = forecast_budget(&rule(), &[40.0, 44.0, 30.0, 20.0, 10.0, 5.0], 5.0, 30);
        assert!(b.trend.is_presentable());
        assert!(!b.trend.corroborated());
        assert!(build_alerts("p1", &[b], &[]).is_empty());
    }

    #[test]
    fn a_corroborated_rise_alerts_and_states_its_confidence() {
        let b = forecast_budget(&rule(), &[5.0, 10.0, 15.0, 20.0, 25.0, 30.0], 30.0, 30);
        let alerts = build_alerts("p1", &[b], &[]);
        let a = alerts.first().expect("a genuine ramp still pages");
        assert!(a.message.contains("r²=") && a.message.contains("over 6 days"));
    }

    #[test]
    fn a_disabled_guardrail_is_not_reported_as_the_policy_in_force() {
        let mut alerts = vec![ForecastAlert {
            kind: "margin_erosion",
            severity: "warning",
            project_id: "p1".into(),
            subject: "acme".into(),
            eta_days: 5.0,
            message: String::new(),
            policy_applied: None,
        }];
        let mut guard = rule();
        guard.id = "g1".into();
        guard.origin = Some("margin-policy:pol-1:acme".into());
        attach_guardrails(&mut alerts, std::slice::from_ref(&guard));
        assert_eq!(alerts[0].policy_applied.as_deref(), Some("g1"));
        guard.enabled = false;
        attach_guardrails(&mut alerts, std::slice::from_ref(&guard));
        assert!(
            alerts[0].policy_applied.is_none(),
            "a switched-off guardrail is a warning again, not a report"
        );
    }

    #[test]
    fn the_dedup_key_lets_an_escalation_through_the_cooldown() {
        let mut a = ForecastAlert {
            kind: "budget_breach",
            severity: "warning",
            project_id: "p1".into(),
            subject: "r1".into(),
            eta_days: 9.0,
            message: String::new(),
            policy_applied: None,
        };
        let warned = a.dedup_key();
        a.severity = "high";
        assert_ne!(
            warned,
            a.dedup_key(),
            "warning → high inside one cooldown is the message worth sending"
        );
    }
}
