//! What a benchmark actually runs against: one row of the comparison matrix.
//!
//! Split out of [`score`](crate::score) (which owns the verdict types) because a target grew from
//! "provider + model + a literal system prompt" into something that can *resolve*: it may name a
//! registry prompt to fetch at run start, and it may not be a model at all but an HTTP endpoint —
//! a whole RAG pipeline behind a URL, which is the thing most teams actually want graded.
//!
//! Both additions are serde-defaulted, so every stored matrix keeps deserializing unchanged.

use serde::{Deserialize, Serialize};

/// Report key carrying the prompt version a run **actually generated with** — written by the runner
/// after it resolved the registry, read by the promotion gate.
///
/// The distinction this constant exists to enforce: the older `prompt_version` key is *provenance*,
/// copied verbatim from the enqueue payload, so it records what a run was asked to score and would
/// be present even if the run never read a prompt. `resolved_prompt_version` is only ever written
/// by the code that fetched the content and handed it to the generator, so a gate that requires it
/// is a gate that has seen its target. Spelled once here because two crates must agree on it.
pub const RESOLVED_PROMPT_VERSION: &str = "resolved_prompt_version";

/// The `{{input}}` placeholder a registry prompt may use to say "the case's input goes *here*".
/// Absent it, the prompt is used as the system prompt and the input stays the user turn.
pub const INPUT_PLACEHOLDER: &str = "{{input}}";

/// A reference to a registry prompt, resolved at run start. Exactly one of `version` / `label` may
/// be given; neither means "the latest version".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRef {
    /// Registry name within the benchmark's project (e.g. `support-reply`).
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl PromptRef {
    /// The `?version=` / `?label=` query the runtime fetch needs (empty = latest).
    pub fn query(&self) -> String {
        match (self.version, self.label.as_deref()) {
            (Some(v), _) => format!("?version={v}"),
            (None, Some(l)) => format!("?label={l}"),
            (None, None) => String::new(),
        }
    }

    /// `Err(reason)` when the reference contradicts itself. A ref pinning both a number and a label
    /// is ambiguous, and silently preferring one would make a gate certify a version nobody named.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("prompt_ref.name must not be empty".into());
        }
        if self.version.is_some() && self.label.is_some() {
            return Err(format!(
                "prompt_ref '{}' sets both `version` and `label`; pass at most one",
                self.name
            ));
        }
        Ok(())
    }
}

/// How a target produces its candidate output.
///
/// `Model` is the historical (and default) shape: call a provider's model. `Http` posts the case to
/// an endpoint the operator owns and reads the answer back — which is how a benchmark reaches a RAG
/// pipeline, an agent, or anything else whose quality does not live in a single model call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TargetKind {
    #[default]
    Model,
    Http {
        url: String,
    },
}

impl TargetKind {
    pub fn is_model(&self) -> bool {
        matches!(self, TargetKind::Model)
    }
}

/// One target in a comparison matrix: a provider+model, optionally with a named system-prompt
/// variant. Stored inline in a benchmark's `target` field as an array (Phase 3.6e).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchTarget {
    pub provider: String,
    pub model: String,
    /// System/instruction prompt variant under test. A literal, and the fallback when no
    /// [`prompt_ref`](Self::prompt_ref) resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Display label; defaults to `provider/model` if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Fetch this target's prompt from the registry at run start, instead of using the literal
    /// `system_prompt`. This is what makes a promotion gate run the version it certifies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_ref: Option<PromptRef>,
    #[serde(default, skip_serializing_if = "TargetKind::is_model")]
    pub kind: TargetKind,
}

impl BenchTarget {
    /// The endpoint this target posts to, when it is an HTTP target.
    pub fn http_url(&self) -> Option<&str> {
        match &self.kind {
            TargetKind::Http { url } => Some(url.as_str()),
            TargetKind::Model => None,
        }
    }

    /// The provider id to use for **family** comparisons (the self-preference bias control).
    ///
    /// For an HTTP target this is its *host*, never the declared `provider`: what answers at
    /// `rag.acme.com` is opaque to us, so claiming it belongs to the judge's family — or to any
    /// family — would be an invention. Two endpoints on the same host compare equal; an endpoint
    /// and a model never do.
    pub fn family_provider(&self) -> String {
        match self.http_url().and_then(url_host) {
            Some(host) => host,
            None => self.provider.clone(),
        }
    }

    /// Display label, falling back to `provider/model`.
    pub fn display_label(&self) -> String {
        self.label
            .clone()
            .unwrap_or_else(|| format!("{}/{}", self.provider, self.model))
    }
}

/// The host of an absolute URL (`https://host:port/path` → `host`), lowercased. Deliberately a
/// small hand parse: `core` has no URL dependency and this only ever sees absolute URLs.
pub fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r)?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // Strip any userinfo, then the port.
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(end) = host.strip_prefix('[') {
        // IPv6 literal: keep the brackets' contents.
        end.split_once(']').map(|(h, _)| h)?
    } else {
        host.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_legacy_target_still_deserializes_and_defaults_to_a_model() {
        let t: BenchTarget =
            serde_json::from_value(json!({ "provider": "openai", "model": "gpt-4o" }))
                .expect("legacy shape");
        assert!(t.kind.is_model());
        assert!(t.prompt_ref.is_none());
        // …and round-trips without inventing the new keys, so a stored matrix is unchanged.
        let back = serde_json::to_value(&t).unwrap();
        assert_eq!(back, json!({ "provider": "openai", "model": "gpt-4o" }));
    }

    #[test]
    fn a_resolvable_target_round_trips() {
        let v = json!({
            "provider": "acme", "model": "rag-v2",
            "prompt_ref": { "name": "support-reply", "label": "production" },
            "kind": { "type": "http", "url": "https://rag.acme.com/answer" }
        });
        let t: BenchTarget = serde_json::from_value(v.clone()).expect("resolvable shape");
        assert_eq!(t.http_url(), Some("https://rag.acme.com/answer"));
        assert_eq!(t.prompt_ref.as_ref().unwrap().query(), "?label=production");
        assert_eq!(serde_json::to_value(&t).unwrap(), v);
    }

    #[test]
    fn an_http_targets_family_is_its_host_not_its_declared_provider() {
        let mut t: BenchTarget =
            serde_json::from_value(json!({ "provider": "anthropic", "model": "x" })).unwrap();
        assert_eq!(t.family_provider(), "anthropic");
        // Declaring `provider: anthropic` on an opaque endpoint must not let it read as the
        // judge's family — the host is the honest answer.
        t.kind = TargetKind::Http {
            url: "https://RAG.acme.com:8443/answer".into(),
        };
        assert_eq!(t.family_provider(), "rag.acme.com");
    }

    #[test]
    fn a_version_wins_over_a_label_and_both_at_once_is_refused() {
        assert_eq!(
            PromptRef {
                name: "p".into(),
                version: Some(3),
                label: None
            }
            .query(),
            "?version=3"
        );
        assert_eq!(
            PromptRef {
                name: "p".into(),
                version: None,
                label: None
            }
            .query(),
            ""
        );
        let both = PromptRef {
            name: "p".into(),
            version: Some(3),
            label: Some("production".into()),
        };
        assert!(both.validate().is_err(), "ambiguous ref is refused");
    }

    #[test]
    fn url_host_handles_ports_userinfo_and_v6() {
        assert_eq!(url_host("https://a.example/x"), Some("a.example".into()));
        assert_eq!(
            url_host("https://u:p@a.example:8443/"),
            Some("a.example".into())
        );
        assert_eq!(url_host("https://[::1]:9/x"), Some("::1".into()));
        assert_eq!(url_host("not-a-url"), None);
    }
}
