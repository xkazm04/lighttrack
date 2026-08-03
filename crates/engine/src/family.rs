//! Model families, for the **self-preference** bias control.
//!
//! `BENCHMARK_FRAMEWORK.md` has documented "judge family != generator family" as one of the four
//! bias controls since the framework was written, and until now nothing in the code knew what a
//! family *was* — so a run could judge Claude output with a Claude judge and report nothing. This
//! module is the missing half: a coarse family label a caller can compare.
//!
//! Deliberately coarse. The signal we need is "did the same lab's model grade its own output?", not
//! a version taxonomy, so `claude-haiku-4-5` and `claude-opus-5` are one family.

/// The lab family behind a `(provider, model)` pair. Falls back to the provider string when the
/// model name carries no recognisable marker — an unknown provider is its own family, which keeps
/// the comparison honest (never silently "different").
pub fn model_family(provider: &str, model: &str) -> String {
    let m = model.to_ascii_lowercase();
    // Model name wins over provider: a gateway/proxy provider can serve another lab's model, and
    // the family that matters for self-preference is whoever trained it.
    if m.contains("claude") || m.starts_with("haiku") || m.starts_with("sonnet") || m.starts_with("opus") {
        return "anthropic".to_string();
    }
    if m.contains("gemini") || m.contains("gemma") {
        return "google".to_string();
    }
    if m.starts_with("gpt") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") {
        return "openai".to_string();
    }
    provider.to_ascii_lowercase()
}

/// True when a judge and the target it is grading come from the same lab — the self-preference
/// condition. Never hard-fails a run: callers warn and record.
pub fn same_family(
    judge_provider: &str,
    judge_model: &str,
    target_provider: &str,
    target_model: &str,
) -> bool {
    model_family(judge_provider, judge_model) == model_family(target_provider, target_model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_are_recognised_from_the_model_name() {
        assert_eq!(model_family("anthropic", "haiku"), "anthropic");
        assert_eq!(model_family("anthropic", "claude-haiku-4-5"), "anthropic");
        assert_eq!(model_family("google", "gemini-2.5-flash"), "google");
        assert_eq!(model_family("openai", "gpt-5-mini"), "openai");
        assert_eq!(model_family("openai", "o3"), "openai");
    }

    #[test]
    fn the_model_name_outranks_a_proxy_provider() {
        // A gateway serving Claude is still the Anthropic family for bias purposes.
        assert_eq!(model_family("openrouter", "anthropic/claude-sonnet-5"), "anthropic");
    }

    #[test]
    fn unknown_models_fall_back_to_the_provider() {
        assert_eq!(model_family("mistral", "mixtral-8x7b"), "mistral");
        // …and an unknown provider is its own family, so it never reads as "different" by accident.
        assert!(same_family("mistral", "mixtral", "mistral", "ministral"));
    }

    #[test]
    fn self_preference_is_detected_across_aliases() {
        assert!(same_family("anthropic", "haiku", "anthropic", "claude-sonnet-5"));
        assert!(!same_family("google", "gemini-2.5-flash", "anthropic", "claude-haiku-4-5"));
    }
}
