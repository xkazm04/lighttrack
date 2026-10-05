//! The ingest door's usage-invariant check: a sub-count never exceeds the total it is a sub-count of.
//!
//! `TokenUsage` documents that `cached_input` is the part of `input` served from a prompt cache — a
//! subset, never an addition — and until this module existed it enforced that nowhere. The price
//! book then consumed the sub-count with a saturating subtraction, so a violating record did not
//! fail loudly: the residual reached zero and the **entire** prompt billed at the cached rate, the
//! deepest possible under-bill, on exactly the traffic with the highest cache ratios. The core type
//! now owns the arithmetic ([`lighttrack_core::usage_split`]); this is the boundary that refuses the
//! impossible state before it is stored, so the volume lands in a rejection with a code instead of
//! in a cost chart nobody disputes.
//!
//! Three things this module deliberately is not:
//!
//! - **It does not repair the record.** Which of the two counts is wrong is unknowable from the
//!   record — only the extractor that produced it knows whether its provider reports cached tokens
//!   inside the total or beside it — so a door that quietly fixed it would be inventing tokens, and
//!   would be the reason the invariant stayed unenforced for the next three years.
//! - **It is not provider knowledge.** One comparison, no table, no family matching: that is what
//!   makes it safe to apply to senders this tree has never seen.
//! - **It is not silent when it is off.** The knob below disables the *refusal*, not the
//!   observation: a disabled rule still counts and logs every violation it saw, so an operator who
//!   turned it off to keep an instrumented sender flowing can still see what that costs them.
//!
//! A sibling module of `events_validate` rather than a section of it: that file is at the ~300-LOC
//! ceiling, and the check has its own knob, its own counter and its own tests.

use std::sync::atomic::{AtomicU64, Ordering};

use lighttrack_core::LlmEvent;

use crate::error::ErrorCode;
use crate::events_validate::Rejection;

/// Env: `0` disables the refusal (the same "`0` means off" shape the ts-skew knobs use). Anything
/// else — including unset — leaves it on. An operator whose OTel sender ships Anthropic's exclusive
/// cache counter can keep ingesting while they fix the exporter, and pay for it knowingly.
pub(crate) const ENV_CHECK_USAGE_SUBCOUNTS: &str = "LIGHTTRACK_CHECK_USAGE_SUBCOUNTS";

/// Violations seen since this process started, refused or admitted alike.
///
/// Process-local and best-effort, exactly like [`crate::rejections::RejectionLedger`] — the thing it
/// buys is that "we turned the rule off" and "nothing was ever wrong" stop looking identical.
static VIOLATIONS_SEEN: AtomicU64 = AtomicU64::new(0);

/// Resolve the knob. Read per call rather than memoized in a `OnceLock`, because the only caller is
/// already behind one ([`crate::events_validate::policy`]) and a second cache would make the flag
/// untestable without a subprocess.
pub(crate) fn enabled_from_env() -> bool {
    match std::env::var(ENV_CHECK_USAGE_SUBCOUNTS) {
        Ok(v) => v.trim() != "0",
        Err(_) => true,
    }
}

/// How many violating records this process has seen. A test hook: the number operators read is the
/// `violations_seen` field on every line below, which is what makes "the rule is off" and "nothing
/// was ever wrong" distinguishable in a log.
#[cfg(test)]
pub(crate) fn violations_seen() -> u64 {
    VIOLATIONS_SEEN.load(Ordering::Relaxed)
}

/// Check one event's usage split. Every violation is counted and logged; `enforcing` decides whether
/// it is also refused.
pub(crate) fn check(ev: &LlmEvent, enforcing: bool) -> Result<(), Rejection> {
    let Err(overflow) = ev.usage.check_subcounts() else {
        return Ok(());
    };
    let seen = VIOLATIONS_SEEN.fetch_add(1, Ordering::Relaxed) + 1;
    if !enforcing {
        tracing::warn!(
            event_id = %ev.id,
            provider = %ev.provider.as_str(),
            model = %ev.model,
            input = overflow.input,
            cached_input = overflow.cached_input,
            violations_seen = seen,
            var = ENV_CHECK_USAGE_SUBCOUNTS,
            "admitting a usage record whose cached sub-count exceeds its total: the check is \
             disabled, so this call's prompt will price as if it were entirely cached",
        );
        return Ok(());
    }
    tracing::debug!(
        event_id = %ev.id,
        provider = %ev.provider.as_str(),
        model = %ev.model,
        input = overflow.input,
        cached_input = overflow.cached_input,
        violations_seen = seen,
        "refusing a usage record whose cached sub-count exceeds its total",
    );
    Err(Rejection::new(
        ErrorCode::UsageSubcountExceedsTotal,
        format!(
            "{overflow}. Add the provider's cache counters into `usage.input` before sending (see \
             the LightTrack SDK extractors), or set {ENV_CHECK_USAGE_SUBCOUNTS}=0 to accept the \
             record unpriced-at-the-input-rate anyway"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(input: u64, cached: Option<u64>) -> LlmEvent {
        let mut usage = json!({ "input": input, "output": 10 });
        if let Some(c) = cached {
            usage["cached_input"] = json!(c);
        }
        serde_json::from_value(json!({
            "id": "e1",
            "provider": "anthropic",
            "model": "claude-haiku-4-5",
            "usage": usage,
        }))
        .unwrap()
    }

    #[test]
    fn refuses_a_subcount_larger_than_its_total_with_its_own_code() {
        let r = check(&ev(100, Some(400)), true).unwrap_err();
        assert_eq!(r.code, ErrorCode::UsageSubcountExceedsTotal);
        assert!(r.message.contains("400"), "{}", r.message);
        assert!(
            r.message.contains(ENV_CHECK_USAGE_SUBCOUNTS),
            "the message names the knob that reverses it: {}",
            r.message
        );
    }

    #[test]
    fn the_legal_shapes_pass_including_a_fully_cached_prompt() {
        // One-sided by design: equality is the ordinary warm-cache case, and silence is not zero.
        for (input, cached) in [(500, Some(500)), (500, Some(0)), (500, None), (0, None)] {
            assert!(
                check(&ev(input, cached), true).is_ok(),
                "input={input} cached={cached:?}"
            );
        }
    }

    #[test]
    fn a_disabled_rule_admits_the_record_and_still_counts_it() {
        let before = violations_seen();
        assert!(
            check(&ev(100, Some(400)), false).is_ok(),
            "the knob disables the refusal"
        );
        // `>` and not `== before + 1`: the counter is process-wide and the test binary is
        // concurrent, so the only honest claim is that this call was observed. A well-formed record
        // cannot move it at all — `check` returns before the increment.
        assert!(
            violations_seen() > before,
            "…but not the observation: a rule that is off must not be invisible"
        );
        assert!(check(&ev(500, Some(500)), false).is_ok());
    }

    #[test]
    fn the_knob_is_off_only_for_an_explicit_zero() {
        // Resolved against the process env, so assert the parse rule rather than mutating it: unset
        // means on, which is the default every deployment that has never heard of this gets.
        assert!(
            enabled_from_env() || std::env::var(ENV_CHECK_USAGE_SUBCOUNTS).as_deref() == Ok("0"),
            "unset or non-zero must leave the check on"
        );
    }
}
