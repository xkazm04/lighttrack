//! Alerts as **rows**, not as process memory.
//!
//! Every alert this product fires — a breached cap, a soft warning, a spend forecast, a dead relay
//! task, an error spike, a quality regression, a finished benchmark, a flushed rejection bucket —
//! used to be a `tracing::warn!` and a `HashMap<String, Instant>` entry. That has three consequences
//! an observability tool cannot own up to: dedup resets on restart, each replica alerts
//! independently (production is multi-instance), and nothing anywhere records whether the alert was
//! actually *delivered*. An [`Alert`] is the durable answer to "what fired, where did it go, did
//! anyone acknowledge it, and what came of it".
//!
//! [`AlertChannel`] is the routing half: a destination, optionally scoped to one project, with a
//! minimum severity and an optional kind filter. Env-configured channels are synthesised as
//! *global* rows at startup rather than persisted, so an existing deployment keeps behaving exactly
//! as it did.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What kind of condition fired. One variant per alert this system has ever produced — the wire
/// literals match the `event` field webhook receivers have always been switching on, so a receiver
/// written before the ledger existed keeps working.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    /// A limit rule breached (`LimitStatus.breached`).
    LimitBreach,
    /// A limit rule crossed its `warn_at` fraction without breaching.
    LimitWarning,
    /// A pre-emptive forecast alert: budget breach ETA or margin erosion.
    ForecastAlert,
    /// A relay task dead-lettered (attempts exhausted, or the device vanished).
    RelayTaskDead,
    /// A burst of failed calls in one project inside the rolling error window.
    ErrorSpike,
    /// The recent mean verdict for one (project, rubric) regressed below its baseline.
    ScoreDrop,
    /// A benchmark run finished (the CI gate contract's completion hook).
    BenchRun,
    /// A periodic flush of the in-process rejection ledger: how many ingest attempts a cap turned
    /// away. Deliberately an alert row and never an event — a rejected call was never stored as an
    /// event precisely because it would corrupt the usage rollups every cap is evaluated against.
    IngestRejected,
    /// The version a prompt label serves is measurably worse than the version it is being compared
    /// against (M23). The one alert that fires *after* a promotion, which is where a registry
    /// previously stopped looking.
    PromptCanaryRegressed,
    /// Queued relay tasks name an action no enrolled device advertises any more (M18): the fleet
    /// changed under work that was routable when it was accepted.
    RelayTaskUnroutable,
}

impl AlertKind {
    pub const ALL: &'static [AlertKind] = &[
        AlertKind::LimitBreach,
        AlertKind::LimitWarning,
        AlertKind::ForecastAlert,
        AlertKind::RelayTaskDead,
        AlertKind::ErrorSpike,
        AlertKind::ScoreDrop,
        AlertKind::BenchRun,
        AlertKind::IngestRejected,
        AlertKind::PromptCanaryRegressed,
        AlertKind::RelayTaskUnroutable,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AlertKind::LimitBreach => "limit_breach",
            AlertKind::LimitWarning => "limit_warning",
            AlertKind::ForecastAlert => "forecast_alert",
            AlertKind::RelayTaskDead => "relay_task_dead",
            AlertKind::ErrorSpike => "error_spike",
            AlertKind::ScoreDrop => "score_drop",
            AlertKind::BenchRun => "bench_run",
            AlertKind::IngestRejected => "ingest_rejected",
            AlertKind::PromptCanaryRegressed => "prompt_canary_regressed",
            AlertKind::RelayTaskUnroutable => "relay_task_unroutable",
        }
    }

    /// Parse a wire literal. `None` for a kind this build does not know — a row written by a newer
    /// release must not hard-fail an older reader's whole list.
    pub fn from_wire(s: &str) -> Option<Self> {
        AlertKind::ALL.iter().copied().find(|k| k.as_str() == s)
    }

    /// The severity this kind fires at unless the caller says otherwise.
    pub fn default_severity(self) -> Severity {
        match self {
            AlertKind::LimitBreach | AlertKind::RelayTaskDead => Severity::Critical,
            AlertKind::LimitWarning
            | AlertKind::ForecastAlert
            | AlertKind::ErrorSpike
            | AlertKind::ScoreDrop
            | AlertKind::PromptCanaryRegressed
            | AlertKind::RelayTaskUnroutable => Severity::Warning,
            AlertKind::BenchRun | AlertKind::IngestRejected => Severity::Info,
        }
    }
}

/// How loud an alert is. Ordered, so a channel's `min_severity` is a `>=` comparison.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    #[default]
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }

    /// Parse a wire literal, defaulting to [`Severity::Info`] for anything unrecognised — a stored
    /// row must never be unreadable because a newer release added a level.
    pub fn from_wire(s: &str) -> Self {
        match s {
            "critical" => Severity::Critical,
            "warning" => Severity::Warning,
            _ => Severity::Info,
        }
    }
}

/// One delivery attempt's outcome, appended to the alert as it fans out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Delivery {
    /// The [`AlertChannel::id`] this went to, or a synthetic id for an env-configured global
    /// channel (`env:webhook`, `env:ntfy`, `env:email`).
    pub channel_id: String,
    pub ok: bool,
    /// HTTP status, or a short transport error when the request never got one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub at: DateTime<Utc>,
}

/// One fired alert, with everything that happened to it.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Alert {
    #[serde(default = "crate::new_id")]
    pub id: String,
    /// The project this concerns, or `None` for a deployment-wide condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub kind: AlertKind,
    /// The logical identity the cooldown dedups on. Two alerts sharing it inside the cooldown are
    /// the same ongoing condition, not two incidents.
    pub dedup_key: String,
    pub severity: Severity,
    /// The structured body the channel delivered (the same JSON a webhook receiver sees).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
    pub fired_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delivered: Vec<Delivery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_by: Option<String>,
    /// What came of it — the responder's diagnosis, or an operator's note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Value>,
}

impl Alert {
    /// A fired alert with a fresh id and `fired_at = now`, at the kind's default severity.
    pub fn new(
        kind: AlertKind,
        project_id: Option<String>,
        dedup_key: String,
        payload: Value,
    ) -> Self {
        Self {
            id: crate::new_id(),
            project_id,
            kind,
            dedup_key,
            severity: kind.default_severity(),
            payload,
            fired_at: Utc::now(),
            delivered: Vec::new(),
            acked_at: None,
            acked_by: None,
            resolution: None,
        }
    }

    pub fn with_severity(mut self, s: Severity) -> Self {
        self.severity = s;
        self
    }

    /// Did every attempted delivery succeed? `false` when nothing was attempted — an alert nobody
    /// received is not a delivered alert.
    pub fn fully_delivered(&self) -> bool {
        !self.delivered.is_empty() && self.delivered.iter().all(|d| d.ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_kind_round_trips_through_its_wire_literal() {
        for k in AlertKind::ALL {
            assert_eq!(AlertKind::from_wire(k.as_str()), Some(*k));
        }
        assert_eq!(AlertKind::from_wire("from_a_newer_release"), None);
    }

    #[test]
    fn severity_orders_so_a_floor_is_a_comparison() {
        assert!(Severity::Critical > Severity::Warning);
        assert!(Severity::Warning > Severity::Info);
        assert_eq!(Severity::from_wire("nonsense"), Severity::Info);
    }

    #[test]
    fn an_alert_with_no_delivery_is_not_delivered() {
        let mut a = Alert::new(
            AlertKind::LimitBreach,
            Some("p1".into()),
            "p1:cost_usd:hour".into(),
            json!({ "breach": true }),
        );
        assert_eq!(a.severity, Severity::Critical, "breaches are critical");
        assert!(!a.fully_delivered());
        a.delivered.push(Delivery {
            channel_id: "env:webhook".into(),
            ok: false,
            status: Some("500".into()),
            at: Utc::now(),
        });
        assert!(!a.fully_delivered());
        a.delivered[0].ok = true;
        assert!(a.fully_delivered());
    }
}
