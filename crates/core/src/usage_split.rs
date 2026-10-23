//! The one home of the `cached_input ⊆ input` split.
//!
//! [`TokenUsage`] states an arithmetic invariant in its doc comment: `input` is the whole prompt the
//! provider counted and `cached_input` is a part of it, never an addition. Every consumer that bills
//! the two halves at different rates has to subtract one from the other, and a `saturating_sub`
//! spread across those call sites is how a violating record turns into a cheap number instead of a
//! refusal — the residual reaches zero and the *entire* prompt prices at the cached rate, the
//! deepest possible under-bill, on exactly the workloads with the highest cache ratios. So the
//! subtraction lives here, once, and it returns a `Result`: a caller cannot perform it without
//! deciding what to do about the impossible case.
//!
//! What this module deliberately does **not** do is repair the record. Which of the two counts is
//! wrong is not knowable from the record — only the extractor that produced it knows whether its
//! provider reports cached tokens inside the total (OpenAI's `prompt_tokens`) or beside it
//! (Anthropic's `input_tokens`) — so inventing the missing tokens here would be the same layering
//! mistake [`crate::event::FailureClass`] exists to refuse. The ingest boundary refuses a violating
//! record; nothing guesses on its behalf.

use std::fmt;

use crate::event::TokenUsage;

/// A usage record whose cached sub-count exceeds the total it is a sub-count of.
///
/// Carries both numbers rather than a message, because every consumer wants to say them: the ingest
/// door puts them in the rejection prose a sender has to debug against, and the price book uses the
/// mere existence of this value to decide the record has no honest price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubcountOverflow {
    /// The reported whole-prompt total.
    pub input: u64,
    /// The reported cached part of it, which was larger.
    pub cached_input: u64,
}

impl SubcountOverflow {
    /// How many tokens the sub-count overshot its total by — the size of the lie, which is also the
    /// number an extractor that forgot to add an exclusive cache counter back in would be missing.
    pub fn excess(&self) -> u64 {
        self.cached_input.saturating_sub(self.input)
    }
}

impl fmt::Display for SubcountOverflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`usage.cached_input` ({}) exceeds `usage.input` ({}) by {} token(s); `cached_input` is \
             the part of the prompt served from cache — a subset of `input`, never an addition",
            self.cached_input,
            self.input,
            self.excess()
        )
    }
}

impl std::error::Error for SubcountOverflow {}

impl TokenUsage {
    /// The cached part of the prompt as a number, `0` when the sender reported none.
    ///
    /// The field itself stays tri-state — `None` ("the sender said nothing") is never *stored* as
    /// `Some(0)` ("the sender measured zero"), which is the difference between an unreported cache
    /// and a cold one — so this is only the billing view of it, taken at the point of arithmetic.
    pub fn cached_tokens(&self) -> u64 {
        self.cached_input.unwrap_or(0)
    }

    /// The one-sided invariant: a sub-count never exceeds the total it is a sub-count of.
    ///
    /// One comparison, no provider knowledge required — which is what makes it checkable at a
    /// boundary that accepts records from senders this tree has never seen.
    pub fn check_subcounts(&self) -> Result<(), SubcountOverflow> {
        match self.cached_input {
            Some(cached) if cached > self.input => Err(SubcountOverflow {
                input: self.input,
                cached_input: cached,
            }),
            _ => Ok(()),
        }
    }

    /// The part of the prompt billed at the **input** rate: `input − cached_input`.
    ///
    /// The only `input - cached` in the workspace. It is a `Result` and not a saturating `u64`
    /// precisely so that no caller can price a violating record by accident: saturation answered
    /// `0` for the impossible case, which reads as "this prompt was entirely cached" — a sentence
    /// no cost chart disputes.
    pub fn billable_input(&self) -> Result<u64, SubcountOverflow> {
        self.check_subcounts()?;
        Ok(self.input - self.cached_tokens())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, cached: Option<u64>) -> TokenUsage {
        TokenUsage {
            input,
            output: 0,
            cached_input: cached,
            reasoning: None,
        }
    }

    #[test]
    fn the_split_is_the_difference_when_the_invariant_holds() {
        assert_eq!(
            usage(1_000_000, Some(500_000)).billable_input(),
            Ok(500_000)
        );
        // The legal boundary case: a fully-cached prompt bills nothing at the input rate.
        assert_eq!(usage(500, Some(500)).billable_input(), Ok(0));
        // An unreported cache is the whole prompt at the input rate, and the field stays `None`.
        let none = usage(500, None);
        assert_eq!(none.billable_input(), Ok(500));
        assert_eq!(none.cached_input, None, "tri-state is not collapsed");
        assert_eq!(none.cached_tokens(), 0, "…but the arithmetic view is 0");
        // An explicit zero is distinguishable from silence and prices identically.
        let zero = usage(500, Some(0));
        assert_eq!(zero.billable_input(), Ok(500));
        assert_eq!(zero.cached_input, Some(0));
    }

    #[test]
    fn a_subcount_larger_than_its_total_is_reported_not_saturated() {
        let bad = usage(100, Some(400));
        let err = bad.billable_input().unwrap_err();
        assert_eq!(
            err,
            SubcountOverflow {
                input: 100,
                cached_input: 400
            }
        );
        assert_eq!(err.excess(), 300);
        // The old shape. Kept as a literal here so the regression is named rather than described:
        // whatever this function returns, it must never be this.
        assert_eq!(
            bad.input.saturating_sub(bad.cached_tokens()),
            0,
            "the saturating arithmetic this type refuses to perform"
        );
        assert!(bad.check_subcounts().is_err());
    }

    #[test]
    fn the_message_names_both_numbers_and_the_direction_of_the_fix() {
        let msg = SubcountOverflow {
            input: 100,
            cached_input: 400,
        }
        .to_string();
        assert!(msg.contains("400"), "{msg}");
        assert!(msg.contains("100"), "{msg}");
        assert!(msg.contains("subset"), "{msg}");
    }
}
