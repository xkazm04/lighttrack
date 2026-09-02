//! One extra alert kind on [`Alerter`]: **`relay_task_unroutable`** (M18).
//!
//! Admission at the enqueue door refuses a task no enrolled device advertises, which catches the
//! typo. It cannot catch the other shape of the same failure: a task that *was* routable when it
//! was accepted and is not any more — the only device that had the action was revoked, went to a
//! narrower capability list on its next upgrade, or was never re-enrolled after a rebuild. Nothing
//! is wrong with that task; there is simply nobody left to run it, and it will sit queued until it
//! ages out of somebody's patience rather than out of a budget.
//!
//! It lives in its own file, like `alerts_canary.rs`, but it is not its own transport. The first
//! version shipped a private copy of the delivery path — its own env read, its own HTTP client, no
//! destination vetting, no signature, no ledger row — because the `alerts` module was being
//! restructured at the same time. That reason expired with the restructure, and the copy was the
//! one alert path a `302` to the cloud metadata service would have walked straight through. The
//! composed [`Alert`] now goes through [`Alerter::fire`]: the same store-backed admission gate, the
//! same per-project routing, the same signed and vetted delivery, the same `GET /v1/alerts` row.

use std::sync::Arc;

use serde::Serialize;
use serde_json::json;

use lighttrack_core::{Alert, AlertKind};

use crate::alerts::Alerter;

/// One action type nothing in the fleet can run, and how much work is stuck behind it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UnroutableActions {
    pub(crate) action_type: String,
    /// How many queued tasks name it.
    pub(crate) tasks: u32,
    /// How long the oldest of them has been waiting, in seconds.
    pub(crate) oldest_secs: i64,
    /// How many devices are enrolled at all — the number that tells an operator whether the fix is
    /// "enrol a device" or "widen one's capabilities".
    pub(crate) enrolled_devices: u32,
}

impl UnroutableActions {
    /// The cooldown identity: one ongoing "nobody can run this" per action type. Deliberately not
    /// per task — the queue behind an action fills and drains as one condition.
    pub(crate) fn dedup_key(&self) -> String {
        format!("relay-unroutable:{}", self.action_type)
    }

    /// The alert text. It names the fix, because "unroutable" on its own sends an operator to the
    /// wrong half of the system: the fleet is what changed, not the app that enqueued the work.
    pub(crate) fn message(&self) -> String {
        let mins = (self.oldest_secs / 60).max(1);
        let fix = if self.enrolled_devices == 0 {
            "no devices are enrolled at all — enrol one (POST /v1/relay/devices)".to_string()
        } else {
            format!(
                "{} device(s) are enrolled and none advertises it — add it to a device's \
                 capabilities, or check that the device that used to run it has not been revoked",
                self.enrolled_devices
            )
        };
        format!(
            "LightTrack alert: {} queued relay task(s) for action '{}' have no eligible device \
             (oldest waiting {}m). {}.",
            self.tasks, self.action_type, mins, fix
        )
    }

    /// The row, in the same `{event, text, content, subject, …}` envelope every other alert uses,
    /// so a receiver switching on `event` keeps working and the numbers ride along with the prose.
    /// Deployment-wide (`project_id: None`): the fleet is not a tenant's.
    fn alert(&self) -> Alert {
        let msg = self.message();
        Alert::new(
            AlertKind::RelayTaskUnroutable,
            None,
            self.dedup_key(),
            json!({
                "event": AlertKind::RelayTaskUnroutable.as_str(),
                "text": msg,
                "content": msg,
                "subject": format!(
                    "LightTrack: relay action '{}' has no eligible device",
                    self.action_type
                ),
                "unroutable": self,
            }),
        )
    }
}

impl Alerter {
    /// Fire best-effort `relay_task_unroutable` alerts through the durable ledger, deduped per
    /// action type on the shared cooldown so a permanently-stuck queue reports once a window rather
    /// than once a sweep.
    pub(crate) fn notify_relay_unroutable(self: &Arc<Self>, stuck: &[UnroutableActions]) {
        if !self.enabled() {
            return;
        }
        let due: Vec<Alert> = stuck
            .iter()
            .filter(|s| self.should_send_key(&s.dedup_key()))
            .map(UnroutableActions::alert)
            .collect();
        if due.is_empty() {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move { me.fire(due).await });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stuck(enrolled: u32) -> UnroutableActions {
        UnroutableActions {
            action_type: "xprice/reprice-summary".into(),
            tasks: 3,
            oldest_secs: 3_600,
            enrolled_devices: enrolled,
        }
    }

    #[test]
    fn the_message_names_the_actual_fix_which_depends_on_whether_a_fleet_exists() {
        // Nobody enrolled: the operator needs to enrol, not to edit a capability list.
        let m = stuck(0).message();
        assert!(m.contains("no devices are enrolled"), "{m}");
        assert!(m.contains("/v1/relay/devices"), "{m}");

        // A fleet that exists and cannot run it: the fix is on a device, and revocation is the
        // likeliest cause of a task that used to route and now does not.
        let m = stuck(2).message();
        assert!(m.contains("2 device(s) are enrolled"), "{m}");
        assert!(m.contains("revoked"), "{m}");
        // Either way the alert names the action and how much is stuck behind it.
        assert!(
            m.contains("xprice/reprice-summary") && m.contains("3 queued"),
            "{m}"
        );
        assert!(m.contains("60m"), "the wait is reported in minutes: {m}");
    }

    /// The payload IS the delivered body: envelope for receivers, evidence for the ledger, and the
    /// kind/severity/key the routing and cooldown decide on.
    #[test]
    fn the_alert_carries_the_envelope_the_evidence_and_a_per_action_key() {
        let a = stuck(2).alert();
        assert_eq!(a.kind, AlertKind::RelayTaskUnroutable);
        assert_eq!(a.payload["event"], "relay_task_unroutable");
        assert_eq!(a.payload["text"], a.payload["content"]);
        assert_eq!(a.payload["unroutable"]["tasks"], 3);
        assert_eq!(a.payload["unroutable"]["enrolled_devices"], 2);
        assert!(
            a.project_id.is_none(),
            "the fleet is deployment-wide, not a tenant's"
        );
        assert_eq!(a.severity, lighttrack_core::Severity::Warning);
        assert_eq!(a.dedup_key, "relay-unroutable:xprice/reprice-summary");
        let mut later = stuck(2);
        later.tasks = 9;
        later.oldest_secs = 7_200;
        assert_eq!(
            later.alert().dedup_key,
            a.dedup_key,
            "the queue behind one action filling further is the same ongoing condition"
        );
    }
}
