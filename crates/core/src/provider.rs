//! Provider identity: an **open** id, plus a coarse lab family.
//!
//! The provider used to be a closed enum whose only escape hatch was `Unknown`, so every unmodeled
//! provider (mistral, bedrock, groq, deepseek, ollama…) was *stored* as the literal `"unknown"` and
//! the operator's real name was lost. That made a price row for `mistral/*` unreachable forever: the
//! book was keyed `unknown/<model>` on lookup and `mistral/<model>` on write.
//!
//! So identity and classification are split:
//! - [`ProviderId`] is the id we persist and key prices/limits on. It is open: any name survives.
//! - [`ProviderFamily`] is the coarse "which lab" label used for *classification only*
//!   (self-preference bias control, judge tagging). Families merge; ids never do.

use std::borrow::Cow;

use serde::{Deserialize, Deserializer, Serialize};

/// The sentinel a provider-less event carries, and what pre-M8 rows were written with when their
/// real provider was outside the old enum. Never a real vendor.
pub const UNKNOWN_PROVIDER: &str = "unknown";

/// A canonical provider id — lowercase, trimmed, restricted to `[a-z0-9._-]`.
///
/// Canonicalized on construction *and* on deserialize, so a wire value, a DB column, and a
/// hand-written price row all reach the same string. Nothing is mapped onto anything else here: an
/// id is preserved verbatim once canonicalized (`mistral` stays `mistral`, `az.ai.openai` stays
/// `az.ai.openai`). Synonym folding is a separate, declared step in
/// [`crate::model_id::canonicalize`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct ProviderId(String);

impl ProviderId {
    /// Canonicalize an arbitrary string into an id. Disallowed characters collapse to `-` (so
    /// `"Azure OpenAI"` becomes `azure-openai` rather than `azureopenai`); an empty result becomes
    /// [`UNKNOWN_PROVIDER`].
    pub fn new(s: &str) -> ProviderId {
        let mut out = String::with_capacity(s.len());
        let mut pending_sep = false;
        for c in s.trim().chars() {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                if pending_sep && !out.is_empty() {
                    out.push('-');
                }
                pending_sep = false;
                out.push(c);
            } else {
                pending_sep = true;
            }
        }
        let trimmed = out.trim_matches('-');
        if trimmed.is_empty() {
            return ProviderId(UNKNOWN_PROVIDER.to_string());
        }
        if trimmed.len() == out.len() {
            ProviderId(out)
        } else {
            ProviderId(trimmed.to_string())
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this id is the "we were never told" sentinel — the only value that is not a claim
    /// about a real vendor.
    pub fn is_unknown(&self) -> bool {
        self.0 == UNKNOWN_PROVIDER
    }

    /// The coarse lab family behind this id.
    pub fn family(&self) -> ProviderFamily {
        family_of(&self.0)
    }
}

impl Default for ProviderId {
    fn default() -> Self {
        ProviderId(UNKNOWN_PROVIDER.to_string())
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ProviderId {
    fn from(s: &str) -> Self {
        ProviderId::new(s)
    }
}

impl From<String> for ProviderId {
    fn from(s: String) -> Self {
        ProviderId::new(&s)
    }
}

impl PartialEq<str> for ProviderId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl<'de> Deserialize<'de> for ProviderId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        // Borrowed where possible; canonicalization happens at the seam so no downstream code has to
        // remember to do it.
        let raw = Cow::<'de, str>::deserialize(d)?;
        Ok(ProviderId::new(&raw))
    }
}

/// The coarse lab behind a provider or model name. Used for classification only — never as a
/// storage key, never as a merge key for prices or leaderboard rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderFamily {
    OpenAi,
    Anthropic,
    Google,
    Mistral,
    Meta,
    /// Anything we don't classify — its own family, so a comparison never reads as "different lab"
    /// by accident.
    #[serde(other)]
    #[default]
    Other,
}

impl ProviderFamily {
    /// The wire label. `Other` is `"unknown"` — the vocabulary the collective digest and the judge
    /// tag have always published.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderFamily::OpenAi => "openai",
            ProviderFamily::Anthropic => "anthropic",
            ProviderFamily::Google => "google",
            ProviderFamily::Mistral => "mistral",
            ProviderFamily::Meta => "meta",
            ProviderFamily::Other => UNKNOWN_PROVIDER,
        }
    }

    pub fn is_known(&self) -> bool {
        !matches!(self, ProviderFamily::Other)
    }
}

impl std::fmt::Display for ProviderFamily {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classify a provider id **or a model name** into a lab family.
///
/// One table, deliberately: a gateway serves another lab's models, so the caller that needs the
/// training lab (`same_family`, the judge tag) asks about the model name with the same function it
/// asks about the provider with. Markers are substrings/prefixes because `gen_ai.system` values are
/// namespaced (`az.ai.openai`, `gcp.gemini`, `vertex_ai`).
pub fn family_of(s: &str) -> ProviderFamily {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() {
        return ProviderFamily::Other;
    }
    // Anthropic markers include the bare model lines (`haiku`, `sonnet`, `opus`) a judge spec uses.
    if s.contains("anthropic")
        || s.contains("claude")
        || starts_any(&s, &["haiku", "sonnet", "opus"])
    {
        return ProviderFamily::Anthropic;
    }
    if s.contains("openai") || s.contains("gpt") || starts_any(&s, &["azure", "o1", "o3", "o4"]) {
        return ProviderFamily::OpenAi;
    }
    if s.contains("google")
        || s.contains("gemini")
        || s.contains("gemma")
        || s.contains("vertex")
        || s.contains("bison")
        || s.starts_with("gcp")
    {
        return ProviderFamily::Google;
    }
    if s.contains("mistral") || s.contains("mixtral") || s.contains("codestral") {
        return ProviderFamily::Mistral;
    }
    if s.contains("llama") || s.starts_with("meta") {
        return ProviderFamily::Meta;
    }
    ProviderFamily::Other
}

/// Whether `s` starts with any marker, at a token boundary (so `o3` matches `o3-mini` but not
/// `o3xyz`, and `opus` doesn't swallow an unrelated `opusfoo`).
fn starts_any(s: &str, markers: &[&str]) -> bool {
    markers.iter().any(|m| match s.strip_prefix(m) {
        Some(rest) => rest.is_empty() || rest.starts_with('-') || rest.starts_with('.'),
        None => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_id_survives_verbatim() {
        // The whole point of M8: an unmodeled provider is not coerced to `unknown`.
        assert_eq!(ProviderId::new("mistral").as_str(), "mistral");
        assert_eq!(ProviderId::new("  Groq ").as_str(), "groq");
        assert_eq!(ProviderId::new("az.ai.openai").as_str(), "az.ai.openai");
        assert_eq!(ProviderId::new("Azure OpenAI").as_str(), "azure-openai");
        assert_eq!(ProviderId::new("").as_str(), UNKNOWN_PROVIDER);
        assert_eq!(ProviderId::new("///").as_str(), UNKNOWN_PROVIDER);
    }

    #[test]
    fn ids_round_trip_through_serde_canonicalized() {
        let p: ProviderId = serde_json::from_str("\"Mistral\"").unwrap();
        assert_eq!(p.as_str(), "mistral");
        assert_eq!(serde_json::to_string(&p).unwrap(), "\"mistral\"");
    }

    #[test]
    fn families_classify_ids_and_model_names() {
        assert_eq!(family_of("az.ai.openai"), ProviderFamily::OpenAi);
        assert_eq!(family_of("azure-openai"), ProviderFamily::OpenAi);
        assert_eq!(family_of("gpt-4o"), ProviderFamily::OpenAi);
        assert_eq!(family_of("o3-mini"), ProviderFamily::OpenAi);
        assert_eq!(family_of("claude-haiku-4-5"), ProviderFamily::Anthropic);
        assert_eq!(family_of("haiku"), ProviderFamily::Anthropic);
        assert_eq!(family_of("gcp.gemini"), ProviderFamily::Google);
        assert_eq!(family_of("vertex_ai"), ProviderFamily::Google);
        assert_eq!(family_of("mixtral-8x7b"), ProviderFamily::Mistral);
        assert_eq!(family_of("llama-3.3-70b"), ProviderFamily::Meta);
        assert_eq!(family_of("some-local-llm"), ProviderFamily::Other);
        assert_eq!(family_of(""), ProviderFamily::Other);
    }

    #[test]
    fn an_id_keeps_its_own_family_but_not_its_neighbours() {
        assert_eq!(
            ProviderId::new("az.ai.openai").family(),
            ProviderFamily::OpenAi
        );
        // …and the id itself is untouched: classification never rewrites identity.
        assert_eq!(ProviderId::new("az.ai.openai").as_str(), "az.ai.openai");
        assert_eq!(ProviderFamily::Other.as_str(), "unknown");
    }
}
