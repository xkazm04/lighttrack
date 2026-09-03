//! Where an alert goes: one routing destination.
//!
//! Split from [`crate::alert`] because it is a different thing with a different lifecycle — an
//! [`Alert`](crate::Alert) is an event that happened, a [`AlertChannel`] is configuration an
//! operator wrote. `project_id: None` means **global**, which is exactly what the env-configured
//! destinations have always been; the API synthesises those rather than storing them, so adding
//! this table changes nothing for a deployment that has not created a channel.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::alert::{AlertKind, Severity};

/// Where an alert goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
}
