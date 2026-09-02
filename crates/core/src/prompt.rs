use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::prompt_canary::{CanaryPolicy, LabelChange, MAX_LABEL_HISTORY};

/// A named entry in the prompt registry. Holds the label→version pointers (e.g.
/// `{"production": 2, "staging": 5}`) fetched at runtime, plus an optional linked benchmark whose
/// regression check gates promotion. The actual prompt text lives in [`PromptVersion`] rows, one per
/// immutable version — so a registry edit is a new version, never an in-place overwrite.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Prompt {
    #[serde(default = "crate::new_id")]
    pub id: String,
    #[serde(default)]
    pub project_id: String,
    /// Registry name, unique per project (e.g. `support-reply`).
    pub name: String,
    /// Benchmark run on each new version; its regression verdict gates label promotion. Reuses the
    /// existing benchmark + job-queue machinery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub benchmark_id: Option<String>,
    /// Label → version pointers, e.g. `{"production": 3}`. A runtime fetch by `label` resolves here.
    #[serde(default)]
    pub labels: BTreeMap<String, u32>,
    /// The online canary policy, when this prompt has one (M23). `None` leaves promotion as the last
    /// thing the registry ever observes about a version — the gap the canary closes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canary: Option<CanaryPolicy>,
    /// Every label move, append-only, oldest first, bounded to [`MAX_LABEL_HISTORY`].
    ///
    /// A label pointer alone answers "what is served now" and nothing else — not what it replaced,
    /// not when, not why — so an automatic revert would be indistinguishable from a human
    /// promotion, and there would be no version for a revert to fall *back* to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub label_history: Vec<LabelChange>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
    #[serde(default = "Utc::now")]
    pub updated_at: DateTime<Utc>,
}

impl Prompt {
    /// Point `label` at `version` and record the move, keeping the ledger bounded.
    ///
    /// One function so the pointer and its history can never disagree: a caller that wrote
    /// `labels.insert(..)` on its own would leave a served version with no record of how it got
    /// there, which is the state the canary exists to end.
    pub fn set_label(&mut self, label: &str, version: u32, reason: &str) {
        self.labels.insert(label.to_string(), version);
        self.label_history.push(LabelChange {
            label: label.to_string(),
            version,
            at: Utc::now(),
            reason: Some(reason.to_string()),
        });
        let excess = self.label_history.len().saturating_sub(MAX_LABEL_HISTORY);
        if excess > 0 {
            self.label_history.drain(..excess);
        }
    }

    /// The version `label` pointed at **before** its current value, read from the ledger. `None`
    /// when the label has never moved here — which is why an auto-revert with no recorded
    /// predecessor does nothing rather than guessing at a version to fall back to.
    pub fn previous_version(&self, label: &str) -> Option<u32> {
        let current = self.labels.get(label).copied();
        self.label_history
            .iter()
            .rev()
            .filter(|c| c.label == label)
            .map(|c| c.version)
            .find(|v| Some(*v) != current)
    }
}

/// One immutable version of a [`Prompt`]. `version` is monotonic per prompt (1, 2, 3, …).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PromptVersion {
    #[serde(default = "crate::new_id")]
    pub id: String,
    #[serde(default)]
    pub prompt_id: String,
    #[serde(default)]
    pub version: u32,
    /// The prompt text / template.
    pub content: String,
    /// Optional structured config (model, params, variable schema). Free-form.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub config: Value,
    /// Optional change note ("commit message") describing why this version was cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt_canary::{REASON_CANARY_REGRESSED, REASON_PROMOTE};

    fn prompt() -> Prompt {
        Prompt {
            id: "pr-1".into(),
            project_id: "p1".into(),
            name: "support-reply".into(),
            benchmark_id: None,
            labels: BTreeMap::new(),
            canary: None,
            label_history: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// A prompt written before the canary existed must read back as one with no policy and no
    /// ledger, not fail to deserialize — the registry is live on three backends.
    #[test]
    fn a_pre_canary_prompt_row_still_deserializes() {
        let p: Prompt = serde_json::from_value(serde_json::json!({
            "id": "pr-1", "project_id": "p1", "name": "support-reply",
            "labels": { "production": 2 },
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
        }))
        .expect("legacy prompt");
        assert!(p.canary.is_none());
        assert!(p.label_history.is_empty());
        assert_eq!(p.labels.get("production"), Some(&2));
    }

    /// The pointer and the ledger move together, so a served version always says how it got there.
    #[test]
    fn setting_a_label_records_why_it_moved() {
        let mut p = prompt();
        p.set_label("production", 2, REASON_PROMOTE);
        assert_eq!(p.labels.get("production"), Some(&2));
        assert_eq!(p.label_history.len(), 1);
        assert_eq!(p.label_history[0].reason.as_deref(), Some(REASON_PROMOTE));
        assert_eq!(p.label_history[0].version, 2);
    }

    /// The revert path's whole premise: a label with no recorded predecessor has nothing to fall
    /// back to, and must say so rather than invent a version.
    #[test]
    fn the_previous_version_comes_from_the_ledger_or_nowhere() {
        let mut p = prompt();
        assert_eq!(p.previous_version("production"), None, "never moved");

        p.set_label("production", 2, REASON_PROMOTE);
        assert_eq!(
            p.previous_version("production"),
            None,
            "one move records where it went, not what it replaced"
        );

        p.set_label("production", 3, REASON_PROMOTE);
        assert_eq!(p.previous_version("production"), Some(2));

        // Another label's history never leaks into this one's rollback target.
        p.set_label("canary", 9, REASON_PROMOTE);
        p.set_label("canary", 10, REASON_CANARY_REGRESSED);
        assert_eq!(p.previous_version("production"), Some(2));
        assert_eq!(p.previous_version("canary"), Some(9));
    }

    /// The ledger rides on a row read by every runtime fetch, so a flapping canary must not be able
    /// to grow it without limit.
    #[test]
    fn the_ledger_is_bounded_and_drops_the_oldest_first() {
        let mut p = prompt();
        for v in 1..=(MAX_LABEL_HISTORY as u32 + 10) {
            p.set_label("canary", v, REASON_PROMOTE);
        }
        assert_eq!(p.label_history.len(), MAX_LABEL_HISTORY);
        assert_eq!(
            p.label_history[0].version, 11,
            "the newest window is kept, the oldest entries drop off the front"
        );
        assert_eq!(
            p.previous_version("canary"),
            Some(MAX_LABEL_HISTORY as u32 + 9)
        );
    }
}
