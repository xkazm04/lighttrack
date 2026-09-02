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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
            | AlertKind::ScoreDrop => Severity::Warning,
            AlertKind::BenchRun | AlertKind::IngestRejected => Severity::Info,
        }
    }
}

/// How loud an alert is. Ordered, so a channel's `min_severity` is a `>=` comparison.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Where an alert goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    /// A JSON POST, signed (see `X-LightTrack-Signature` in `docs/ALERTS.md`).
    Webhook,
    /// A plain-text POST to an ntfy topic URL.
    Ntfy,
    /// Resend's REST API; `target` is the recipient address.
    Email,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Webhook => "webhook",
            ChannelKind::Ntfy => "ntfy",
            ChannelKind::Email => "email",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "webhook" => Some(ChannelKind::Webhook),
            "ntfy" => Some(ChannelKind::Ntfy),
            "email" => Some(ChannelKind::Email),
            _ => None,
        }
    }
}

/// One routing destination. `project_id: None` means **global** — it receives every project's
/// alerts, which is exactly what the env-configured channels have always been.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertChannel {
    #[serde(default = "crate::new_id")]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub kind: ChannelKind,
    /// The URL (webhook/ntfy) or address (email) alerts are sent to.
    pub target: String,
    /// The **derived signing key** for this channel's webhook signature: `sha256(secret)` as hex.
    ///
    /// The plaintext secret is minted server-side, returned exactly once on create (mirroring the
    /// API-key show-once pattern) and never stored, so a database leak does not hand out the shared
    /// secret an operator may have reused. A receiver derives the same key from the secret it was
    /// shown. Never serialized outward — see [`AlertChannel::redacted`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_hash: Option<String>,
    /// The previous signing key, kept live through a rotation so in-flight receivers that have not
    /// picked up the new secret still verify. Deliveries carry a `v1=` value for each.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_secret_hash: Option<String>,
    #[serde(default)]
    pub min_severity: Severity,
    /// Which kinds this channel wants. Empty = every kind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<AlertKind>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

impl AlertChannel {
    /// Should this channel receive `alert`? Severity floor first, then the kind filter — an empty
    /// filter means "everything", which is what a channel created without one is asking for.
    pub fn accepts(&self, kind: AlertKind, severity: Severity) -> bool {
        self.enabled
            && severity >= self.min_severity
            && (self.kinds.is_empty() || self.kinds.contains(&kind))
    }

    /// A copy safe to hand back over HTTP: the signing keys are stripped, because a channel read
    /// is not a secret read.
    pub fn redacted(&self) -> Self {
        Self {
            secret_hash: None,
            prev_secret_hash: None,
            ..self.clone()
        }
    }

    /// Reject a destination that cannot mean anything for this kind before it is ever stored.
    /// Scheme/address vetting is a *delivery-time* concern and lives in the API (`alerts::vet`); this
    /// is the shape check the store depends on.
    pub fn validate(&self) -> Result<(), String> {
        if self.target.trim().is_empty() {
            return Err("channel target must not be empty".into());
        }
        match self.kind {
            ChannelKind::Webhook | ChannelKind::Ntfy => {
                if !self.target.starts_with("http://") && !self.target.starts_with("https://") {
                    return Err(format!(
                        "{} target must be an http(s) URL, got '{}'",
                        self.kind.as_str(),
                        self.target
                    ));
                }
            }
            ChannelKind::Email => {
                if !self.target.contains('@') {
                    return Err(format!("email target '{}' is not an address", self.target));
                }
            }
        }
        Ok(())
    }
}

fn yes() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn channel(kind: ChannelKind, target: &str) -> AlertChannel {
        AlertChannel {
            id: "c1".into(),
            project_id: None,
            kind,
            target: target.into(),
            secret_hash: Some("deadbeef".into()),
            prev_secret_hash: None,
            min_severity: Severity::Info,
            kinds: Vec::new(),
            enabled: true,
            created_at: Utc::now(),
        }
    }

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
    fn a_floor_and_a_kind_filter_both_narrow() {
        let mut c = channel(ChannelKind::Webhook, "https://example.test/hook");
        c.min_severity = Severity::Critical;
        assert!(c.accepts(AlertKind::LimitBreach, Severity::Critical));
        assert!(!c.accepts(AlertKind::LimitWarning, Severity::Warning));

        c.min_severity = Severity::Info;
        c.kinds = vec![AlertKind::ScoreDrop];
        assert!(c.accepts(AlertKind::ScoreDrop, Severity::Info));
        assert!(
            !c.accepts(AlertKind::LimitBreach, Severity::Critical),
            "a kind filter excludes even a louder alert"
        );

        c.enabled = false;
        c.kinds.clear();
        assert!(!c.accepts(AlertKind::ScoreDrop, Severity::Critical));
    }

    /// Handing a channel back over HTTP must never hand back what signs its deliveries.
    #[test]
    fn redaction_strips_both_signing_keys() {
        let mut c = channel(ChannelKind::Webhook, "https://example.test/hook");
        c.prev_secret_hash = Some("f00d".into());
        let r = c.redacted();
        assert!(r.secret_hash.is_none() && r.prev_secret_hash.is_none());
        assert_eq!(r.target, c.target);
    }

    #[test]
    fn a_target_that_cannot_mean_anything_is_refused() {
        assert!(channel(ChannelKind::Webhook, "https://ok.test/h")
            .validate()
            .is_ok());
        assert!(channel(ChannelKind::Webhook, "ops@example.test")
            .validate()
            .is_err());
        assert!(channel(ChannelKind::Email, "ops@example.test")
            .validate()
            .is_ok());
        assert!(channel(ChannelKind::Email, "https://ok.test/h")
            .validate()
            .is_err());
        assert!(channel(ChannelKind::Ntfy, "  ").validate().is_err());
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
