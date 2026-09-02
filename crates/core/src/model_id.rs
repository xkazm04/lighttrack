//! The **one** canonicalizer for `(provider, model)` identity.
//!
//! Before M8 there were four: `Provider::from_wire`, the price book's date-suffix trim, the
//! collective alias table, and the judge-family sniffing in the scorecard. Each knew a different
//! subset of the truth, so a price row, a limit scope, and a leaderboard row could disagree about
//! whether two calls were the same model.
//!
//! The algorithm, in order, and deliberately small:
//! 1. **provider synonyms** — a short *declared* table (`azure-openai` → `openai`). Only exact
//!    matches fold; nothing is inferred from a family, because folding on family would merge a
//!    gateway's traffic into the lab it proxies.
//! 2. **`provider/model` prefix** — `openai/gpt-4o` → `gpt-4o` (and the prefix seeds the provider
//!    when the caller had none).
//! 3. **lane split** — `@batch` / `@flex` / `@in>N` come off as a [`ModelId::lane`].
//! 4. **date suffix** — `-20251001` and `-2024-08-06` come off as a [`ModelId::variant`], so a cap
//!    or price on `gpt-4o` covers `gpt-4o-2024-08-06`.
//! 5. **declared aliases** — the per-model `aliases` lists in the price seed
//!    (`config/pricing.json`), for the collapses no rule can derive (`gemini-2.5-pro-002`).
//!
//! Steps 1–4 are total and pure; step 5 needs the table and lives in [`canonicalize_with`].

use crate::alias_table::AliasTable;
use crate::provider::{family_of, ProviderFamily, ProviderId};

/// Declared provider synonyms. Exact, case-folded matches only — see the module note on why this is
/// not `family_of`.
const PROVIDER_SYNONYMS: &[(&str, &str)] = &[
    ("azure-openai", "openai"),
    ("azure", "openai"),
    ("google-vertex", "google"),
    ("vertex", "google"),
    ("vertex_ai", "google"),
    ("vertex-ai", "google"),
    ("gemini", "google"),
    ("anthropic-bedrock", "anthropic"),
    ("bedrock-anthropic", "anthropic"),
];

/// A canonical model identity.
///
/// [`ModelId::family`] is the merge key — what a price row, a `LimitScope::Model` and a leaderboard
/// row are compared on. [`ModelId::variant`] and [`ModelId::lane`] are what was *removed* to get
/// there, kept so a caller can tell `gpt-4o` from `gpt-4o-2024-08-06` when it needs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelId {
    pub provider: ProviderId,
    /// The canonical model name (lowercased, undated, lane-free, alias-resolved).
    pub family: String,
    /// The dated point release that was trimmed off (`2024-08-06`), if any.
    pub variant: Option<String>,
    /// The pricing lane that was split off (`batch`, `flex`, `in>200000`), if any.
    pub lane: Option<String>,
}

impl ModelId {
    /// `"<provider>/<family>"` — the price-book / rollup key.
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider.as_str(), self.family)
    }

    /// The lab behind this identity: the model name wins over the provider, because a gateway serves
    /// another lab's models and the family that matters (self-preference, judge tagging) is whoever
    /// trained it.
    pub fn provider_family(&self) -> ProviderFamily {
        match family_of(&self.family) {
            ProviderFamily::Other => self.provider.family(),
            f => f,
        }
    }

    /// The model as written after the derivable steps but *before* the date trim — `gpt-4o` becomes
    /// `gpt-4o-2024-08-06` again. Aliases may be declared on either spelling.
    fn dated(&self) -> String {
        match &self.variant {
            Some(v) => format!("{}-{}", self.family, v),
            None => self.family.clone(),
        }
    }

    /// Apply a declared alias table to an already-canonicalized id (step 5).
    ///
    /// The dated spelling is tried first, so an operator can declare that one *specific* release
    /// belongs elsewhere; only then the undated family. Without that order a table entry on a dated
    /// name could never fire, because step 4 had already trimmed the date away.
    pub fn with_aliases(mut self, aliases: &AliasTable) -> Self {
        let provider = self.provider.as_str().to_string();
        if self.variant.is_some() {
            if let Some(target) = aliases.resolve(&provider, &self.dated()) {
                self.family = target.to_string();
                self.variant = None;
                return self;
            }
        }
        if let Some(target) = aliases.resolve(&provider, &self.family) {
            self.family = target.to_string();
        }
        self
    }
}

/// Canonicalize `(provider, model)` through the derivable rules (steps 1–4).
pub fn canonicalize(provider: &str, model: &str) -> ModelId {
    let mut model = model.trim().to_ascii_lowercase();

    // A `pre/rest` model carries its own provider claim; keep it only when both halves are real, so
    // a name that merely contains a slash is never turned into "".
    let mut prefix: Option<String> = None;
    if let Some((pre, rest)) = model.split_once('/') {
        if !pre.trim().is_empty() && !rest.trim().is_empty() {
            prefix = Some(pre.trim().to_string());
            model = rest.trim().to_string();
        }
    }

    let mut id = ProviderId::new(provider);
    if id.is_unknown() {
        if let Some(p) = &prefix {
            id = ProviderId::new(p);
        }
    }
    let provider = synonym(&id);

    let (rest, lane) = match model.split_once('@') {
        Some((head, tail)) if !head.is_empty() && !tail.is_empty() => {
            (head.to_string(), Some(tail.to_string()))
        }
        _ => (model, None),
    };
    let (family, variant) = split_date_suffix(&rest);

    ModelId {
        provider,
        family,
        variant,
        lane,
    }
}

/// [`canonicalize`] plus the declared alias table (step 5) — the full algorithm.
pub fn canonicalize_with(provider: &str, model: &str, aliases: &AliasTable) -> ModelId {
    canonicalize(provider, model).with_aliases(aliases)
}

/// The coarse judge family behind a judge spec (`[provider/]model`). An explicit, recognized
/// `provider/` prefix wins; otherwise the model name decides. Provider-level only — never the model
/// — because the collective publishes this and a full judge model would be a fingerprint.
pub fn judge_family(spec: &str) -> ProviderFamily {
    let spec = spec.trim();
    if spec.is_empty() {
        return ProviderFamily::Other;
    }
    if let Some((pre, rest)) = spec.split_once('/') {
        let f = family_of(pre);
        if f.is_known() {
            return f;
        }
        if !rest.trim().is_empty() {
            return family_of(rest);
        }
    }
    family_of(spec)
}

/// Fold a provider id through the declared synonym table.
fn synonym(id: &ProviderId) -> ProviderId {
    match PROVIDER_SYNONYMS.iter().find(|(k, _)| *k == id.as_str()) {
        Some((_, canon)) => ProviderId::new(canon),
        None => id.clone(),
    }
}

/// Split a trailing date suffix — `-YYYYMMDD` or `-YYYY-MM-DD` — off a model name.
fn split_date_suffix(model: &str) -> (String, Option<String>) {
    let b = model.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    // `-YYYY-MM-DD` (11 chars) first: its last 3 chars would otherwise not look like a date at all.
    if b.len() > 11 {
        let s = b.len() - 11;
        if b[s] == b'-'
            && b[s + 5] == b'-'
            && b[s + 8] == b'-'
            && digits(s + 1..s + 5)
            && digits(s + 6..s + 8)
            && digits(s + 9..b.len())
        {
            return (model[..s].to_string(), Some(model[s + 1..].to_string()));
        }
    }
    if b.len() > 9 {
        let s = b.len() - 9;
        if b[s] == b'-' && digits(s + 1..b.len()) {
            return (model[..s].to_string(), Some(model[s + 1..].to_string()));
        }
    }
    (model.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_providers_survive_and_synonyms_fold() {
        let id = canonicalize("mistral", "Mistral-Large");
        assert_eq!(id.provider.as_str(), "mistral");
        assert_eq!(id.family, "mistral-large");
        assert_eq!(
            canonicalize("azure-openai", "gpt-4o").provider.as_str(),
            "openai"
        );
        assert_eq!(
            canonicalize("google-vertex", "gemini").provider.as_str(),
            "google"
        );
        // Not a declared synonym → the raw id survives, classification notwithstanding.
        assert_eq!(
            canonicalize("az.ai.openai", "gpt-4o").provider.as_str(),
            "az.ai.openai"
        );
    }

    #[test]
    fn dates_and_lanes_split_off_the_family() {
        let a = canonicalize("openai", "gpt-4o-2024-08-06");
        assert_eq!(a.family, "gpt-4o");
        assert_eq!(a.variant.as_deref(), Some("2024-08-06"));
        let b = canonicalize("anthropic", "claude-haiku-4-5-20251001");
        assert_eq!(b.family, "claude-haiku-4-5");
        assert_eq!(b.variant.as_deref(), Some("20251001"));
        let c = canonicalize("google", "gemini-2.5-pro@in>200000");
        assert_eq!(c.family, "gemini-2.5-pro");
        assert_eq!(c.lane.as_deref(), Some("in>200000"));
        // A version-looking tail that isn't a date is left alone.
        assert_eq!(
            canonicalize("google", "gemini-1.5-pro-002").family,
            "gemini-1.5-pro-002"
        );
        assert_eq!(canonicalize("openai", "gpt-4.1").family, "gpt-4.1");
    }

    #[test]
    fn a_model_prefix_seeds_a_missing_provider_but_never_overrides_one() {
        let a = canonicalize("", "openai/gpt-4o");
        assert_eq!(a.provider.as_str(), "openai");
        assert_eq!(a.family, "gpt-4o");
        let b = canonicalize("openrouter", "anthropic/claude-sonnet-5");
        assert_eq!(b.provider.as_str(), "openrouter");
        assert_eq!(b.family, "claude-sonnet-5");
        // …and the *family* still reads the lab that trained it, not the gateway.
        assert_eq!(b.provider_family(), ProviderFamily::Anthropic);
    }

    #[test]
    fn declared_aliases_apply_last() {
        let t = AliasTable::from_pairs([
            ("gemini-2.5-pro-002", "gemini-2.5-pro"),
            ("google/g-legacy", "gemini-2.5-flash"),
        ]);
        assert_eq!(
            canonicalize_with("google", "gemini-2.5-pro-002", &t).family,
            "gemini-2.5-pro"
        );
        assert_eq!(
            canonicalize_with("google", "g-legacy", &t).family,
            "gemini-2.5-flash"
        );
        // An alias declared on the *dated* spelling still fires, though step 4 trimmed the date.
        let dated = AliasTable::from_pairs([("gpt-4o-2024-08-06", "house-blend")]);
        let id = canonicalize_with("openai", "gpt-4o-2024-08-06", &dated);
        assert_eq!(id.family, "house-blend");
        assert_eq!(id.variant, None, "the alias consumed the dated spelling");
        assert_eq!(
            canonicalize_with("openai", "gpt-4o-2024-05-13", &dated).family,
            "gpt-4o",
            "…and only for the release it names"
        );
        // Undeclared identities pass through untouched.
        assert_eq!(
            canonicalize_with("google", "gemini-9-ultra", &t).family,
            "gemini-9-ultra"
        );
    }

    #[test]
    fn judge_families_read_prefix_then_model() {
        assert_eq!(
            judge_family("anthropic/claude-haiku-4-5"),
            ProviderFamily::Anthropic
        );
        assert_eq!(judge_family("haiku"), ProviderFamily::Anthropic);
        assert_eq!(judge_family("gpt-4o"), ProviderFamily::OpenAi);
        assert_eq!(judge_family("openai/o3-mini"), ProviderFamily::OpenAi);
        assert_eq!(judge_family("gemini-1.5-pro"), ProviderFamily::Google);
        assert_eq!(judge_family("some-local-llm"), ProviderFamily::Other);
        assert_eq!(judge_family("  "), ProviderFamily::Other);
        // An unrecognized gateway prefix defers to the model name.
        assert_eq!(
            judge_family("openrouter/claude-opus-5"),
            ProviderFamily::Anthropic
        );
    }
}
