//! What the device reports back on settle, and the run's **identity**: which prompt text actually
//! ran.
//!
//! The relay is the one LLM workload LightTrack originates, and until now its settle record could
//! not say which prompt produced the result — `prompt.md` is edited in place on disk with no
//! version and no fingerprint, so an action could regress silently for months while the cloud saw
//! an unchanged `action_type`. So every report carries `prompt_sha256` over the **rendered** prompt
//! (params substituted — the text the model actually read), and optionally the action's declared
//! `version`.
//!
//! The prompt and result text themselves are a different question, and the answer is opt-in.
//! Shipping them by default would move an action's real content into the cloud on an upgrade
//! nobody asked for; without them the run is unjudgeable. `report_io` is how an action says which
//! it wants: off, the cloud holds the fingerprint only.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// What the device reports back on settle (mirrors the result endpoint's body).
pub(crate) struct RunReport {
    /// `succeeded` | `failed` | `deferred`.
    pub status: &'static str,
    pub result: Value,
    pub error: Option<String>,
    pub retry_after_secs: Option<u32>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub latency_ms: Option<u64>,
    /// What the CLI envelope said this run cost. The device reports it; the cloud still prices the
    /// relay event its own way (docs/RELAY.md), so this is evidence, not a bill.
    pub cost_usd: Option<f64>,
    /// The posture the run actually executed under — the cloud only ever named an `action_type`,
    /// so without this the settle record cannot say whether a repository was touched.
    pub mode: Option<&'static str>,
    /// Which prompt text ran: sha256 of the rendered prompt, lowercase hex. `None` only when the
    /// run failed before a prompt existed (an unknown or unreadable action).
    pub prompt_sha256: Option<String>,
    /// The action's declared `version`, if it declares one.
    pub action_version: Option<String>,
    /// The rendered prompt — present only when the action set `report_io`.
    pub rendered_prompt: Option<String>,
    /// The result as text — present only when the action set `report_io`.
    pub result_text: Option<String>,
}

impl RunReport {
    pub(crate) fn failed(error: String) -> Self {
        RunReport {
            status: "failed",
            result: Value::Null,
            error: Some(error),
            retry_after_secs: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            latency_ms: None,
            cost_usd: None,
            mode: None,
            prompt_sha256: None,
            action_version: None,
            rendered_prompt: None,
            result_text: None,
        }
    }

    pub(crate) fn deferred(reason: String) -> Self {
        RunReport {
            status: "deferred",
            ..Self::failed(reason)
        }
    }

    /// Stamp the run's identity onto a report however it ended.
    ///
    /// Applied after the fact, on purpose: a failure is exactly the outcome you want the
    /// fingerprint for ("which prompt version started failing"), and every early-return path in
    /// `exec` would otherwise have to remember to carry it.
    pub(crate) fn stamp(mut self, id: &PromptIdentity) -> Self {
        self.prompt_sha256 = Some(id.prompt_sha256.clone());
        self.action_version = id.action_version.clone();
        self.rendered_prompt = id.rendered_prompt.clone();
        self
    }
}

/// The rendered prompt's identity, computed once per run before anything is spawned.
pub(crate) struct PromptIdentity {
    pub prompt_sha256: String,
    pub action_version: Option<String>,
    /// `Some` only under `report_io`.
    pub rendered_prompt: Option<String>,
}

impl PromptIdentity {
    pub(crate) fn new(prompt: &str, version: Option<&str>, report_io: bool) -> Self {
        Self {
            prompt_sha256: sha256_hex(prompt),
            action_version: version.map(str::to_string),
            rendered_prompt: report_io.then(|| prompt.to_string()),
        }
    }
}

/// Lowercase hex sha256 — the fingerprint the cloud groups actions by.
pub(crate) fn sha256_hex(input: &str) -> String {
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    h.finalize().iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fingerprint_is_plain_sha256_hex() {
        // The canonical empty-string digest: a wrong hash function is caught by a constant, not by
        // "it looks like hex".
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(sha256_hex("Hello world").len(), 64);
        assert_ne!(sha256_hex("Hello world"), sha256_hex("Hello  world"));
    }

    /// The fingerprint is over the RENDERED prompt, and the payload is opt-in — the two halves of
    /// the privacy default. Off, the cloud can still tell that the prompt changed.
    #[test]
    fn identity_fingerprints_always_and_carries_text_only_when_opted_in() {
        let closed = PromptIdentity::new("Price A-1 for Acme", Some("v3"), false);
        assert_eq!(closed.prompt_sha256, sha256_hex("Price A-1 for Acme"));
        assert_eq!(closed.action_version.as_deref(), Some("v3"));
        assert!(closed.rendered_prompt.is_none());

        let open = PromptIdentity::new("Price A-1 for Acme", None, true);
        assert_eq!(open.prompt_sha256, closed.prompt_sha256);
        assert_eq!(open.rendered_prompt.as_deref(), Some("Price A-1 for Acme"));
        assert!(open.action_version.is_none());
    }

    /// A failure is the outcome the fingerprint matters most for.
    #[test]
    fn a_failed_report_still_names_its_prompt() {
        let id = PromptIdentity::new("look at {{params.x}}", Some("2"), false);
        let r = RunReport::failed("claude: boom".into()).stamp(&id);
        assert_eq!(r.status, "failed");
        assert_eq!(r.prompt_sha256, Some(id.prompt_sha256.clone()));
        assert_eq!(r.action_version.as_deref(), Some("2"));
        assert!(r.rendered_prompt.is_none());
    }
}
