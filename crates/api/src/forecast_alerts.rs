//! Turning a forecast into pre-emptive warnings: the `ForecastAlert` type, its dedup key, and the
//! pure `budgets + margins -> alerts` mapping.
//!
//! Split out of [`super::forecast`] so the wiring module stays under the file-size bar and so the
//! scheduled sweep ([`crate::forecast_sweep`]) and the `GET /v1/forecast` handler provably build
//! their alerts from one function rather than two that could drift.

use serde::Serialize;

use lighttrack_core::forecast::{BudgetForecast, MarginForecast};
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
    pub(crate) fn dedup_key(&self) -> String {
        format!(
            "forecast:{}:{}:{}",
            self.project_id, self.kind, self.subject
        )
    }
}

pub(crate) fn build_alerts(
    project: &str,
    budgets: &[BudgetForecast],
    margins: &[MarginForecast],
) -> Vec<ForecastAlert> {
    let mut out = Vec::new();
    for b in budgets {
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
                     projected ~{:.4}/day, current rolling {:.4}",
                    window_word(b.window),
                    metric_word(b.metric),
                    b.threshold,
                    humanize(eta),
                    b.projected_daily,
                    b.current,
                ),
            });
        }
    }
    for m in margins {
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
                         rising to ~${:.2}/day",
                        m.key,
                        humanize(eta),
                        m.revenue_per_day,
                        m.cost_per_day
                    ),
                });
            }
        } else if m.cost_trend.slope > 0.0 {
            out.push(ForecastAlert {
                kind: "margin_erosion",
                severity: "high",
                project_id: project.to_string(),
                subject: m.key.clone(),
                eta_days: 0.0,
                policy_applied: None,
                message: format!(
                    "'{}' is already unprofitable (margin ${:.2}) and cost is still rising",
                    m.key, m.margin_usd
                ),
            });
        }
    }
    out
}

/// Stamp `policy_applied` on every `margin_erosion` alert whose subject has a policy-raised
/// guardrail standing. Applied after [`build_alerts`] because only the caller has read the rules;
/// keeping the mapping pure is what lets it be tested without a store.
pub(crate) fn attach_guardrails(
    alerts: &mut [ForecastAlert],
    rules: &[lighttrack_core::LimitRule],
) {
    for a in alerts.iter_mut().filter(|a| a.kind == "margin_erosion") {
        a.policy_applied =
            crate::margin_guardrails::guardrail_for(rules, &a.subject).map(str::to_string);
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
