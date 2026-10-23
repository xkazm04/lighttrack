//! A generation target as the gateway names it: `provider/model[@effort]`.
//!
//! The effort suffix is left on the model spec on purpose — the engine's dispatch is the one
//! place it is split into a typed level (see `providers.rs` there), and re-parsing it here would
//! be a second parser to keep honest. What this module owns is the provider/model split and the
//! *seat* the target spends, which is what failover reasons about: a fallback is only worth trying
//! if it is metered separately from the primary.

use lighttrack_core::{family_of, ProviderFamily};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub provider: String,
    /// `model` or `model@effort`, exactly as the engine's `generate` accepts it.
    pub model: String,
}

impl Target {
    /// Parse `provider/model[@effort]`. A spec with no slash is refused: the gateway never guesses
    /// a provider, because the two seats it fronts are metered separately and a guess would land
    /// the spend on the wrong one.
    pub fn parse(spec: &str) -> Result<Target, String> {
        let spec = spec.trim();
        let Some((provider, model)) = spec.split_once('/') else {
            return Err(format!(
                "target '{spec}' must be 'provider/model[@effort]' \
                 (e.g. anthropic/claude-sonnet-5@medium or codex/gpt-5.5)"
            ));
        };
        let (provider, model) = (provider.trim(), model.trim());
        if provider.is_empty() || model.is_empty() {
            return Err(format!("target '{spec}' has an empty provider or model"));
        }
        Ok(Target {
            provider: provider.to_string(),
            model: model.to_string(),
        })
    }

    /// `provider/model@effort` — the identity a response and an event carry.
    pub fn spec(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }

    /// Which lab's seat this target spends. `codex` is matched on its id like the engine does: the
    /// family table says `Other` for a CLI, but the seat behind it is OpenAI's.
    pub fn family(&self) -> ProviderFamily {
        if self.provider == "codex" {
            return ProviderFamily::OpenAi;
        }
        family_of(&self.provider)
    }

    /// A fallback earns its place only when it is metered apart from what it replaces.
    pub fn same_seat(&self, other: &Target) -> bool {
        if self.provider == other.provider {
            return true;
        }
        let (a, b) = (self.family(), other.family());
        // Two unclassified providers are two different seats until proven otherwise.
        a == b && a != ProviderFamily::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_model_and_keeps_the_effort_suffix_for_the_engine() {
        let t = Target::parse("anthropic/claude-sonnet-5@medium").unwrap();
        assert_eq!(t.provider, "anthropic");
        assert_eq!(t.model, "claude-sonnet-5@medium");
        assert_eq!(t.spec(), "anthropic/claude-sonnet-5@medium");
    }

    #[test]
    fn refuses_a_spec_without_a_provider() {
        assert!(Target::parse("gpt-5.5").is_err());
        assert!(Target::parse("/gpt-5.5").is_err());
        assert!(Target::parse("codex/").is_err());
    }

    #[test]
    fn codex_is_an_openai_seat_and_anthropic_is_not() {
        let codex = Target::parse("codex/gpt-5.5").unwrap();
        let claude = Target::parse("anthropic/claude-sonnet-5").unwrap();
        let openai = Target::parse("openai/gpt-5.5").unwrap();
        assert_eq!(codex.family(), ProviderFamily::OpenAi);
        assert!(codex.same_seat(&openai));
        assert!(!codex.same_seat(&claude));
    }
}
