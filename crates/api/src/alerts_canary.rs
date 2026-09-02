//! One extra alert kind on [`Alerter`]: **`prompt_canary_regressed`** (M23).
//!
//! The alert that fires *after* a promotion — the point at which the registry previously stopped
//! looking. Everything durable is reused rather than rebuilt: the composed [`Alert`] goes through
//! [`Alerter::fire`], so it takes the same store-backed admission gate, the same per-project
//! routing, the same delivery record and the same `GET /v1/alerts` surface every other alert does.
//!
//! It lives in its own file so the `alerts` module is not restructured for one kind, in the same
//! spirit as `alerts_relay.rs` — but unlike that one it does **not** duplicate the channel config:
//! `fire` is `pub(crate)`, so the durable path was reachable from here without touching the module.

use std::sync::Arc;

use serde::Serialize;
use serde_json::json;

use lighttrack_core::{Alert, AlertKind};

use crate::alerts::Alerter;

/// One prompt whose canary label is measurably worse than its production label.
///
/// Both intervals are carried, not just the means. An operator asked to trust a rollback needs to
/// see that the two did not overlap — a bare "0.71 vs 0.78" is the shape of alert people learn to
/// dismiss, because most of the time it is noise.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CanaryRegression {
    pub(crate) project_id: String,
    pub(crate) prompt: String,
    pub(crate) canary_label: String,
    pub(crate) production_label: String,
    pub(crate) canary_version: u32,
    pub(crate) production_version: u32,
    pub(crate) canary_mean: f64,
    pub(crate) production_mean: f64,
    pub(crate) canary_ci95_high: f64,
    pub(crate) production_ci95_low: f64,
    pub(crate) canary_n: u64,
    pub(crate) production_n: u64,
    /// The relative drop that tripped the policy (`0.12` = 12% worse than production).
    pub(crate) drop: f64,
    pub(crate) max_drop: f64,
    /// The version the canary label was moved back to, when `auto_revert` was on and the ledger
    /// named a predecessor. `None` = the alert is the only action taken.
    pub(crate) reverted_to: Option<u32>,
}

impl CanaryRegression {
    /// The cooldown/dedup identity: one ongoing regression per (project, prompt, label), with no
    /// version in the key on purpose — a canary that regresses, is reverted, and is re-promoted is
    /// the same conversation with the operator, not three of them.
    pub(crate) fn dedup_key(&self) -> String {
        format!(
            "prompt-canary:{}:{}:{}",
            self.project_id, self.prompt, self.canary_label
        )
    }

    /// The alert text. It names the decision an operator has to make (or the one already made),
    /// because "canary regressed" alone sends them to `/v1/scores` to work out what happened.
    pub(crate) fn message(&self) -> String {
        let action = match self.reverted_to {
            Some(v) => format!(
                "'{}' has been moved back to v{} automatically",
                self.canary_label, v
            ),
            None => format!(
                "auto-revert is off, so '{}' still points at v{} — move it back with POST \
                 /v1/projects/{}/prompts/{}/promote",
                self.canary_label, self.canary_version, self.project_id, self.prompt
            ),
        };
        format!(
            "LightTrack alert: prompt '{}' v{} ({}) is scoring {:.1}% below v{} ({}) in project \
             '{}' — {:.3} vs {:.3} over {}/{} verdicts, and the intervals do not overlap. {}.",
            self.prompt,
            self.canary_version,
            self.canary_label,
            self.drop * 100.0,
            self.production_version,
            self.production_label,
            self.project_id,
            self.canary_mean,
            self.production_mean,
            self.canary_n,
            self.production_n,
            action,
        )
    }

    fn alert(&self) -> Alert {
        let msg = self.message();
        Alert::new(
            AlertKind::PromptCanaryRegressed,
            Some(self.project_id.clone()),
            self.dedup_key(),
            json!({
                "event": AlertKind::PromptCanaryRegressed.as_str(),
                "text": msg,
                "content": msg,
                "subject": format!("LightTrack: prompt canary regressed in '{}'", self.project_id),
                "canary": self,
            }),
        )
    }
}

impl Alerter {
    /// Fire best-effort `prompt_canary_regressed` alerts through the durable ledger.
    ///
    /// Returns how many were **raised**, not how many were delivered — the cooldown and the store's
    /// admission decide that, which is why a sustained regression alerts an operator once a window
    /// while the sweep keeps finding it every tick.
    pub(crate) fn notify_prompt_canary(
        self: &Arc<Self>,
        regressions: &[CanaryRegression],
    ) -> usize {
        if !self.enabled() {
            return 0;
        }
        let due: Vec<Alert> = regressions
            .iter()
            .filter(|r| self.should_send_key(&r.dedup_key()))
            .map(CanaryRegression::alert)
            .collect();
        let n = due.len();
        if n == 0 {
            return 0;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move { me.fire(due).await });
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regression(reverted: Option<u32>) -> CanaryRegression {
        CanaryRegression {
            project_id: "proj-a".into(),
            prompt: "support-reply".into(),
            canary_label: "canary".into(),
            production_label: "production".into(),
            canary_version: 5,
            production_version: 4,
            canary_mean: 0.62,
            production_mean: 0.81,
            canary_ci95_high: 0.68,
            production_ci95_low: 0.77,
            canary_n: 40,
            production_n: 900,
            drop: 0.2345,
            max_drop: 0.05,
            reverted_to: reverted,
        }
    }

    #[test]
    fn the_message_names_the_evidence_and_the_action() {
        let m = regression(None).message();
        assert!(
            m.contains("support-reply") && m.contains("v5") && m.contains("v4"),
            "{m}"
        );
        assert!(m.contains("23.4% below"), "the size of the drop: {m}");
        assert!(m.contains("40/900 verdicts"), "the evidence: {m}");
        assert!(
            m.contains("auto-revert is off") && m.contains("/promote"),
            "with auto-revert off the alert must name the manual fix: {m}"
        );

        let m = regression(Some(4)).message();
        assert!(
            m.contains("moved back to v4 automatically"),
            "with auto-revert on it must say what was already done: {m}"
        );
        assert!(!m.contains("auto-revert is off"), "{m}");
    }

    /// The key must not fork by version: a canary that regresses, is reverted and is re-promoted is
    /// one ongoing conversation, and a per-version key would restart the cooldown every round.
    #[test]
    fn the_dedup_key_is_stable_across_versions_and_scoped_per_label() {
        let a = regression(None);
        let mut b = regression(Some(4));
        b.canary_version = 9;
        assert_eq!(a.dedup_key(), b.dedup_key());
        assert_eq!(a.dedup_key(), "prompt-canary:proj-a:support-reply:canary");

        let mut other_label = regression(None);
        other_label.canary_label = "staging".into();
        assert_ne!(a.dedup_key(), other_label.dedup_key());
    }

    /// The payload IS the delivered body, so a receiver switching on `event` keeps working and the
    /// evidence rides along with the prose.
    #[test]
    fn the_payload_carries_the_envelope_and_the_numbers() {
        let alert = regression(Some(4)).alert();
        assert_eq!(alert.kind, AlertKind::PromptCanaryRegressed);
        assert_eq!(alert.project_id.as_deref(), Some("proj-a"));
        assert_eq!(alert.payload["event"], "prompt_canary_regressed");
        assert_eq!(alert.payload["canary"]["canary_version"], 5);
        assert_eq!(alert.payload["canary"]["reverted_to"], 4);
        assert!(alert.payload["text"]
            .as_str()
            .is_some_and(|s| !s.is_empty()));
        // A quality regression is a warning, not a page: nothing is down.
        assert_eq!(alert.severity, lighttrack_core::Severity::Warning);
    }
}
