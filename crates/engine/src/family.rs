//! Model families, for the **self-preference** bias control.
//!
//! `BENCHMARK_FRAMEWORK.md` has documented "judge family != generator family" as one of the four
//! bias controls since the framework was written, and until now nothing in the code knew what a
//! family *was* — so a run could judge Claude output with a Claude judge and report nothing. This
//! module is the missing half: a coarse family label a caller can compare.
//!
//! Deliberately coarse. The signal we need is "did the same lab's model grade its own output?", not
//! a version taxonomy, so `claude-haiku-4-5` and `claude-opus-5` are one family.
//!
//! The rules themselves live in `lighttrack_core` (M8) — this is the engine's view of the *one*
//! identity algorithm, not a fourth copy of it.

use lighttrack_core::{canonicalize, ProviderFamily};

/// The lab family behind a `(provider, model)` pair. The model name wins over the provider (a
/// gateway serves another lab's models); an unclassifiable pair falls back to the provider id's own
/// family, which keeps the comparison honest — [`ProviderFamily::Other`] never reads as "different"
/// by accident, because two unknowns compare equal only when their ids agree (see [`same_family`]).
pub fn model_family(provider: &str, model: &str) -> ProviderFamily {
    canonicalize(provider, model).provider_family()
}

/// True when a judge and the target it is grading come from the same lab — the self-preference
/// condition. Never hard-fails a run: callers warn and record.
///
/// Two *unclassified* models are the same family only when their provider ids match, so a local
/// `mistral` judge grading a local `mistral` generation is still flagged, while `groq` judging
/// `ollama` is not.
pub fn same_family(
    judge_provider: &str,
    judge_model: &str,
    target_provider: &str,
    target_model: &str,
) -> bool {
    let judge = model_family(judge_provider, judge_model);
    let target = model_family(target_provider, target_model);
    if judge != target {
        return false;
    }
    if judge.is_known() {
        return true;
    }
    canonicalize(judge_provider, judge_model).provider
        == canonicalize(target_provider, target_model).provider
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_are_recognised_from_the_model_name() {
        assert_eq!(
            model_family("anthropic", "haiku"),
            ProviderFamily::Anthropic
        );
        assert_eq!(
            model_family("anthropic", "claude-haiku-4-5"),
            ProviderFamily::Anthropic
        );
        assert_eq!(
            model_family("google", "gemini-2.5-flash"),
            ProviderFamily::Google
        );
        assert_eq!(model_family("openai", "gpt-5-mini"), ProviderFamily::OpenAi);
        assert_eq!(model_family("openai", "o3"), ProviderFamily::OpenAi);
    }

    #[test]
    fn the_model_name_outranks_a_proxy_provider() {
        // A gateway serving Claude is still the Anthropic family for bias purposes.
        assert_eq!(
            model_family("openrouter", "anthropic/claude-sonnet-5"),
            ProviderFamily::Anthropic
        );
    }

    #[test]
    fn unknown_models_fall_back_to_the_provider() {
        // Mistral is a modeled family now; an id nothing classifies stays `Other`…
        assert_eq!(
            model_family("mistral", "mixtral-8x7b"),
            ProviderFamily::Mistral
        );
        assert_eq!(model_family("acme-labs", "zoo-1"), ProviderFamily::Other);
        // …and two unclassified models are "same family" only when the provider id agrees.
        assert!(same_family("acme-labs", "zoo-1", "acme-labs", "zoo-2"));
        assert!(!same_family("acme-labs", "zoo-1", "other-labs", "zoo-2"));
        assert!(same_family("mistral", "mixtral", "mistral", "ministral"));
    }

    #[test]
    fn self_preference_is_detected_across_aliases() {
        assert!(same_family(
            "anthropic",
            "haiku",
            "anthropic",
            "claude-sonnet-5"
        ));
        assert!(!same_family(
            "google",
            "gemini-2.5-flash",
            "anthropic",
            "claude-haiku-4-5"
        ));
    }
}
