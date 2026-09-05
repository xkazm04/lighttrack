//! The use-case registry: the declared inventory of the places an application calls an LLM.
//!
//! ## Why a registry, when `events.name` already exists
//!
//! `events.name` has carried an "optional use-case / call-site label" since M2, and it is a free
//! string. That is enough to *group* traffic and not enough to *monitor* it, for the same reason
//! `scores.rubric_id` had to exist beside the free-text label — as that field's own doc puts it,
//! it is "the join the free-text label could never be". A label answers "what did these calls call
//! themselves". A registry answers three questions a label cannot:
//!
//! 1. **What is supposed to exist.** Without a declared set there is no denominator, so "we have
//!    telemetry for 9 call sites" is unfalsifiable — it could be complete coverage or a third of it.
//! 2. **What is running that nobody declared.** An event whose `name` matches no registered key is
//!    *shadow usage*: a call site that shipped without anyone registering it, or a typo quietly
//!    splitting one use case's cost across two rollup keys. Both are invisible when the label is the
//!    only thing that exists, because every string is equally valid.
//! 3. **What is running on a model nobody chose.** [`UseCase::expected_models`] is the declaration;
//!    the events say what actually ran. The difference is model drift.
//!
//! The registry is therefore deliberately **not** a foreign key on `events`. Ingest must never
//! reject a call because its use case is unregistered — dropping observability data to enforce
//! paperwork is precisely backwards, and the unregistered rows are themselves the most interesting
//! signal (see 2 above). `key` joins `events.name` by convention, and the gap between declared and
//! observed is a *report*, not a constraint.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What kind of work the model is doing at this call site.
///
/// Not an attempt at a taxonomy of AI: it is the smallest split that changes how a call should be
/// *judged* and what it should *cost*. A classification that returns one token and a report that
/// returns two pages are not comparable on latency, price or quality, and a monitoring layer that
/// averages them together produces a number describing nothing.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum UseCaseKind {
    /// Free-form prose or code for a person or another system to read.
    #[default]
    Generation,
    /// A choice from a closed set.
    Classification,
    /// Structured fields pulled out of unstructured input.
    Extraction,
    /// A shorter faithful rendering of a longer input.
    Summarization,
    /// A model scoring or ranking another model's output (LLM-as-judge).
    Judge,
    /// A multi-step loop that calls tools and decides what to do next.
    Agent,
    /// Vectors for retrieval.
    Embedding,
    /// Re-ordering candidates against a query.
    Rerank,
    /// Anything the list above does not describe. Kept so an unmodeled call site can still be
    /// registered — an unregistrable one would just go unregistered, which defeats the point.
    #[serde(other)]
    Other,
}

impl UseCaseKind {
    pub fn as_str(self) -> &'static str {
        match self {
            UseCaseKind::Generation => "generation",
            UseCaseKind::Classification => "classification",
            UseCaseKind::Extraction => "extraction",
            UseCaseKind::Summarization => "summarization",
            UseCaseKind::Judge => "judge",
            UseCaseKind::Agent => "agent",
            UseCaseKind::Embedding => "embedding",
            UseCaseKind::Rerank => "rerank",
            UseCaseKind::Other => "other",
        }
    }
}

/// Where this call site is in its life.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum UseCaseStatus {
    /// Shipped and expected to produce traffic. Silence here is a finding.
    #[default]
    Active,
    /// Declared but not shipped yet. Silence is expected; **traffic** is the finding.
    Planned,
    /// On its way out. Traffic is expected to fall to zero, and until it does the removal is not
    /// finished — which is a fact worth being able to state rather than assume.
    Deprecated,
}

impl UseCaseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            UseCaseStatus::Active => "active",
            UseCaseStatus::Planned => "planned",
            UseCaseStatus::Deprecated => "deprecated",
        }
    }
}

/// One declared place an application calls an LLM.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UseCase {
    #[serde(default = "crate::new_id")]
    pub id: String,
    #[serde(default)]
    pub project_id: String,
    /// The stable identifier events attribute to, unique per project.
    ///
    /// This is the value an SDK puts in `events.name`, so it is not a display string: it travels in
    /// URLs and is compared exactly. See [`validate_key`].
    pub key: String,
    /// Human title, for a dashboard row.
    pub name: String,
    /// What this call site is for, in a sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub kind: UseCaseKind,
    #[serde(default)]
    pub status: UseCaseStatus,
    /// Where it lives in the application — a module, route or service name.
    ///
    /// Free text on purpose: this is one self-hosted deployment describing its own codebase, so
    /// there is nothing to standardize against and a constrained vocabulary would just be wrong in
    /// somebody else's architecture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    /// The models this call site is *meant* to run, as `[provider/]model` ids.
    ///
    /// Empty means "no declaration", which is different from "any model is fine" — a drift report
    /// says `undeclared` for the first and nothing at all for the second, rather than inventing a
    /// violation out of an absence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_models: Vec<String>,
    /// Who to ask about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
    #[serde(default = "Utc::now")]
    pub updated_at: DateTime<Utc>,
}

/// Longest accepted `key`. It is a URL path segment and an `events.name` value, so it is bounded
/// for the same reasons a project id is.
pub const MAX_KEY_LEN: usize = 64;

/// Validate a use-case key.
///
/// The alphabet is the intersection of "safe in a URL path segment" and "safe as a literal in a
/// filter" — the same reasoning that constrains project ids. A key with a space in it would be
/// accepted by the database and then quietly fail to match the events that carry it, which is the
/// worst of the available failures: a registry entry that looks connected and is not.
pub fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("use-case key must not be blank".to_string());
    }
    if key.len() > MAX_KEY_LEN {
        return Err(format!(
            "use-case key must be at most {MAX_KEY_LEN} characters (got {})",
            key.len()
        ));
    }
    if !key
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return Err(format!(
            "use-case key {key:?} must start with an ASCII letter or digit"
        ));
    }
    if let Some(bad) = key
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':')))
    {
        return Err(format!(
            "use-case key {key:?} contains {bad:?}; allowed: letters, digits, '-', '_', '.', ':'"
        ));
    }
    Ok(())
}

impl UseCase {
    /// Validate everything the API refuses at the door.
    pub fn validate(&self) -> Result<(), String> {
        validate_key(&self.key)?;
        if self.name.trim().is_empty() {
            return Err("use-case name must not be blank".to_string());
        }
        for m in &self.expected_models {
            if m.trim().is_empty() {
                return Err("expected_models must not contain a blank entry".to_string());
            }
        }
        Ok(())
    }

    /// Whether `model` is one this call site declared.
    ///
    /// `None` when nothing was declared — the caller must not read that as a violation. An absent
    /// declaration and a broken one are different states, and collapsing them is how a drift report
    /// starts crying wolf on every unconfigured project in the fleet.
    pub fn declares(&self, model: &str) -> Option<bool> {
        if self.expected_models.is_empty() {
            return None;
        }
        Some(self.expected_models.iter().any(|m| m == model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn uc(key: &str) -> UseCase {
        UseCase {
            id: "u1".into(),
            project_id: "p".into(),
            key: key.into(),
            name: "Scan calibration".into(),
            description: None,
            kind: UseCaseKind::Judge,
            status: UseCaseStatus::Active,
            component: None,
            expected_models: vec![],
            owner: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// The key is compared exactly against `events.name`, so anything that could match one place
    /// and not the other is refused rather than stored.
    #[test]
    fn a_key_that_could_not_match_an_event_is_refused() {
        for bad in [
            "",
            "has space",
            "-leading-dash",
            "slash/in/key",
            "emoji🙂",
            "tab\tinside",
        ] {
            validate_key(bad).unwrap_err();
        }
        for good in ["scan.calibrate", "support_reply", "a", "v2:judge", "job-1"] {
            validate_key(good).unwrap_or_else(|e| panic!("{good}: {e}"));
        }
    }

    #[test]
    fn an_over_long_key_is_refused_with_its_length() {
        let e = validate_key(&"a".repeat(MAX_KEY_LEN + 1)).unwrap_err();
        assert!(e.contains(&(MAX_KEY_LEN + 1).to_string()), "{e}");
    }

    /// No declaration is not a violation. This is the difference between a drift report worth
    /// reading and one that flags every project that never filled the field in.
    #[test]
    fn an_absent_model_declaration_is_not_a_violation() {
        let bare = uc("scan.calibrate");
        assert_eq!(bare.declares("openai/gpt-5.4"), None);

        let mut declared = uc("scan.calibrate");
        declared.expected_models = vec!["openai/gpt-5.4".into()];
        assert_eq!(declared.declares("openai/gpt-5.4"), Some(true));
        assert_eq!(declared.declares("zai-org/GLM-5.3-Flash"), Some(false));
    }

    #[test]
    fn a_blank_name_is_refused() {
        let mut u = uc("k");
        u.name = "  ".into();
        assert!(u.validate().unwrap_err().contains("name"));
    }

    /// Optional fields stay off the wire, so a minimal registration round-trips unchanged.
    #[test]
    fn a_minimal_use_case_round_trips() {
        let src = json!({
            "id": "u1", "project_id": "p", "key": "scan.calibrate", "name": "Scan calibration",
            "kind": "judge", "status": "active",
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
        });
        let u: UseCase = serde_json::from_value(src.clone()).expect("use case");
        assert_eq!(u.kind, UseCaseKind::Judge);
        assert_eq!(serde_json::to_value(&u).expect("re-serialize"), src);
    }

    /// An unmodeled kind must not fail the whole registration — an unregistrable call site simply
    /// goes unregistered, which is the outcome this table exists to prevent.
    #[test]
    fn an_unknown_kind_degrades_to_other_rather_than_failing() {
        let u: UseCase = serde_json::from_value(json!({
            "key": "k", "name": "n", "kind": "telepathy"
        }))
        .expect("unknown kinds are absorbed");
        assert_eq!(u.kind, UseCaseKind::Other);
    }

    #[test]
    fn vocabulary_strings_are_stable() {
        for (k, s) in [
            (UseCaseKind::Generation, "generation"),
            (UseCaseKind::Classification, "classification"),
            (UseCaseKind::Extraction, "extraction"),
            (UseCaseKind::Summarization, "summarization"),
            (UseCaseKind::Judge, "judge"),
            (UseCaseKind::Agent, "agent"),
            (UseCaseKind::Embedding, "embedding"),
            (UseCaseKind::Rerank, "rerank"),
            (UseCaseKind::Other, "other"),
        ] {
            assert_eq!(k.as_str(), s);
            assert_eq!(serde_json::to_value(k).expect("kind"), json!(s));
        }
        for (st, s) in [
            (UseCaseStatus::Active, "active"),
            (UseCaseStatus::Planned, "planned"),
            (UseCaseStatus::Deprecated, "deprecated"),
        ] {
            assert_eq!(st.as_str(), s);
            assert_eq!(serde_json::to_value(st).expect("status"), json!(s));
        }
    }
}
