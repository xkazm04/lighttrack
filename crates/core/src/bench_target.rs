//! What a benchmark actually runs against: one row of the comparison matrix.
//!
//! Split out of [`score`](crate::score) (which owns the verdict types) because a target grew from
//! "provider + model + a literal system prompt" into something that can *resolve*: it may name a
//! registry prompt to fetch at run start, and it may not be a model at all but an HTTP endpoint —
//! a whole RAG pipeline behind a URL, which is the thing most teams actually want graded.
//!
//! Both additions are serde-defaulted, so every stored matrix keeps deserializing unchanged.

use serde::{Deserialize, Serialize};

use crate::effort::{split_effort, Effort};

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
    /// Reasoning effort to run this target at — the axis that makes "is `gpt-5@high` worth 4×
    /// `gpt-5@low` on MY cases?" a question a matrix can ask. Serde-defaulted, so a stored matrix
    /// without it deserializes and re-serializes byte-identically.
    ///
    /// Redundant with an `@effort` suffix on [`model`](Self::model), which is the older spelling and
    /// still works; when both are present this field wins, because it is the more specific
    /// declaration. [`resolved_effort`](Self::resolved_effort) is the one place that rule lives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "TargetKind::is_model")]
    pub kind: TargetKind,
    /// Per-case cost/latency ceilings this target must hold to, beside the rubric's quality bar. A
    /// case that breaches one **fails**, however well it scored — see [`crate::case_limits`].
    /// Compare mode only; serde-defaulted, so a stored matrix without it is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<crate::case_limits::CaseLimits>,
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

    /// The effort this target actually runs at: the declared [`effort`](Self::effort) if set, else
    /// an `@effort` suffix carried on the model spec. `None` means "the provider's default", which
    /// is a *different* thing from any named level and is never inferred as one.
    pub fn resolved_effort(&self) -> Option<Effort> {
        self.effort.or_else(|| split_effort(&self.model).1)
    }

    /// The model id with no effort attached — what a provider's `model` field wants.
    pub fn bare_model(&self) -> &str {
        split_effort(&self.model).0
    }

    /// The spec to hand the generation engine: `model@effort` when an effort applies, else the bare
    /// model. The engine splits this again at the provider boundary, so this is the one string that
    /// carries both halves through an API whose shape predates the effort axis — the same spelling
    /// the use-case registry already writes (`expected_models: ["opus@xhigh"]`).
    pub fn model_spec(&self) -> String {
        match self.resolved_effort() {
            Some(e) => format!("{}@{}", self.bare_model(), e.as_str()),
            None => self.bare_model().to_string(),
        }
    }

    /// Display label, falling back to `provider/model` — or `provider/model@effort` when the target
    /// declares an effort, because two rows of one model at two efforts are the whole point of the
    /// axis and a leaderboard that printed them identically would be unreadable. A matrix with no
    /// effort keeps the exact string it had.
    pub fn display_label(&self) -> String {
        self.label.clone().unwrap_or_else(|| {
            format!(
                "{}/{}",
                self.provider,
                match self.resolved_effort() {
                    Some(e) => format!("{}@{}", self.bare_model(), e.as_str()),
                    None => self.model.clone(),
                }
            )
        })
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

    /// **The axis's reason to exist.** The same model twice at two efforts must be two
    /// distinguishable rows — otherwise "is xhigh worth 4× low on my cases?" has no answer, because
    /// the scorecard prints one label twice.
    #[test]
    fn two_efforts_of_one_model_are_distinguishable() {
        let low: BenchTarget = serde_json::from_value(
            json!({ "provider": "openai", "model": "gpt-5", "effort": "low" }),
        )
        .unwrap();
        let high: BenchTarget = serde_json::from_value(
            json!({ "provider": "openai", "model": "gpt-5", "effort": "xhigh" }),
        )
        .unwrap();
        assert_eq!(low.display_label(), "openai/gpt-5@low");
        assert_eq!(high.display_label(), "openai/gpt-5@xhigh");
        assert_ne!(low.display_label(), high.display_label());
        assert_eq!(low.model_spec(), "gpt-5@low");
        assert_eq!(high.model_spec(), "gpt-5@xhigh");
        assert_eq!(low.bare_model(), "gpt-5");
    }

    /// The older spelling — the level suffixed onto the model, which is what the use-case registry
    /// writes — resolves the same way, and an explicit `effort` field beats it because it is the
    /// more specific declaration. Neither may ever produce `model@a@b`.
    #[test]
    fn the_declared_effort_wins_over_a_suffix_and_never_doubles_it() {
        let suffixed: BenchTarget =
            serde_json::from_value(json!({ "provider": "anthropic", "model": "opus@xhigh" }))
                .unwrap();
        assert_eq!(suffixed.resolved_effort(), Some(Effort::XHigh));
        assert_eq!(suffixed.model_spec(), "opus@xhigh");
        assert_eq!(suffixed.display_label(), "anthropic/opus@xhigh");

        let mut both = suffixed.clone();
        both.effort = Some(Effort::Low);
        assert_eq!(both.resolved_effort(), Some(Effort::Low));
        assert_eq!(both.model_spec(), "opus@low", "no double suffix");
    }

    /// **The stored-matrix guarantee.** A benchmark saved before this axis existed must come back
    /// out of serde byte-identical — no invented `effort` key, and the label it always printed.
    #[test]
    fn a_matrix_without_an_effort_key_round_trips_unchanged() {
        let stored = json!({
            "provider": "openai", "model": "gpt-4o", "system_prompt": "be terse",
            "label": "baseline"
        });
        let t: BenchTarget = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(t.resolved_effort(), None, "absence is not a level");
        assert_eq!(serde_json::to_value(&t).unwrap(), stored);
        assert_eq!(t.display_label(), "baseline");
        // …and with no explicit label, the pre-effort default string is untouched.
        let mut unlabelled = t.clone();
        unlabelled.label = None;
        assert_eq!(unlabelled.display_label(), "openai/gpt-4o");
        assert_eq!(unlabelled.model_spec(), "gpt-4o");
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
