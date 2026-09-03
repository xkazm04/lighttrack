//! Bounded exponential-backoff retries for transient provider failures (429 / 5xx / timeout). The
//! judge is *unbudgeted*, so a rate-limited or briefly-overloaded endpoint is worth a few jittered
//! retries rather than dropping the sample. Classification is by typed [`EngineError`] variant — never
//! by string-matching provider messages.

use std::time::{Duration, Instant};

use crate::{EngineError, Result};

/// Total attempts (1 initial + 2 retries).
const MAX_TRIES: u32 = 3;
/// First backoff step; doubles each retry, plus jitter.
const BASE_DELAY_MS: u64 = 200;
/// Wall-clock budget for ONE provider call *including* its retries. `MAX_TRIES` bounds how many
/// attempts a ladder spends; this bounds how long it may spend them over — the two are different
/// questions, and only the second one answers "a provider asked for five minutes". Distinct from
/// the runner's dollar ceiling (`crates/runner/src/budget.rs`), which asks "can we afford it"
/// where this asks "can we wait for it".
const CALL_BUDGET: Duration = Duration::from_secs(60);
/// The slice of budget a further attempt needs to be worth starting. A sleep that leaves less than
/// this behind buys nothing: the attempt after it would be cut off by the deadline anyway.
const MIN_ATTEMPT_BUDGET: Duration = Duration::from_secs(1);
/// Ceiling on the jitter added to a *stated* wait. Jitter still matters here — sixty benchmark
/// workers told "5s" by the same limiter would otherwise wake in one synchronized pulse — but it
/// only ever delays past what the provider asked, never before it, and it stays small so the wait
/// remains recognisably the one that was stated.
const STATED_JITTER_CAP_MS: u64 = 1_000;

impl EngineError {
    /// Transient failures worth retrying: rate limits, 5xx, and timeouts. Auth/bad-request/parse
    /// failures are deterministic and are surfaced immediately. [`EngineError::OverBudgetWait`] is
    /// deliberately absent: it is a *terminal* state the ladder itself produces, and retrying it
    /// would re-ask a provider that already named a wait we could not hold.
    pub(crate) fn is_retryable(&self) -> bool {
        matches!(
            self,
            EngineError::RateLimited { .. }
                | EngineError::ServerError { .. }
                | EngineError::Timeout { .. }
        )
    }

    /// The delay this failure's response *stated*, if it stated one. The dependency's own schedule
    /// outranks anything the local ladder computes.
    fn stated_retry_after(&self) -> Option<Duration> {
        match self {
            EngineError::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// Who failed — carried into the terminal state so a run report names the provider, not "a
    /// call".
    fn who(&self) -> String {
        match self {
            EngineError::RateLimited { who, .. }
            | EngineError::ServerError { who, .. }
            | EngineError::Timeout { who, .. } => who.clone(),
            other => other.to_string(),
        }
    }

    /// A provider that produced no completion text (distinct from output that failed to parse).
    pub(crate) fn is_empty_completion(&self) -> bool {
        matches!(self, EngineError::EmptyCompletion { .. })
    }
}

/// Run `f`, retrying transient failures with bounded, jittered exponential backoff under
/// [`CALL_BUDGET`]. Non-retryable errors (and successes) return immediately.
pub(crate) fn with_retry<T>(f: impl FnMut() -> Result<T>) -> Result<T> {
    with_retry_within(CALL_BUDGET, f)
}

/// [`with_retry`] against an explicit wall-clock budget.
///
/// Two bounds, and the ladder names which one stopped it:
/// - **exhausted** — `MAX_TRIES` spent, or our own computed ladder no longer fits the clock. The
///   underlying transient failure is returned, because that failure is the story.
/// - **over-budget wait** — the provider *stated* a delay that does not fit. The wait is neither
///   shortened nor the budget stretched: truncating would retry earlier than the provider asked
///   (spending an attempt it has already said will fail, against the very allowance the wait was
///   protecting), and stretching would hand our latency guarantee to whoever is having the
///   incident. See `crates/engine-http/src/lib.rs:917-930` in `pumper` for the fleet's reference
///   spelling of this rule.
pub(crate) fn with_retry_within<T>(
    budget: Duration,
    mut f: impl FnMut() -> Result<T>,
) -> Result<T> {
    let deadline = Instant::now() + budget;
    let mut attempt = 1;
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if e.is_retryable() && attempt < MAX_TRIES => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                // The stated schedule replaces the computed rung outright — it does not merely cap
                // it — and is debited from the budget like any other delay.
                let stated = e.stated_retry_after();
                let delay = match stated {
                    Some(d) => d.saturating_add(Duration::from_millis(jitter(
                        (d.as_millis() as u64).min(STATED_JITTER_CAP_MS),
                    ))),
                    None => backoff(attempt),
                };
                if delay.saturating_add(MIN_ATTEMPT_BUDGET) > remaining {
                    return Err(match stated {
                        // Its own terminal state, carrying the wait that did not fit — the number an
                        // operator needs to tell a wrong budget from a sick provider. Folding this
                        // into the rate-limit error would destroy exactly that fact.
                        Some(asked) => EngineError::OverBudgetWait {
                            who: e.who(),
                            asked_secs: asked.as_secs_f64(),
                            remaining_secs: remaining.as_secs_f64(),
                            attempts: attempt,
                        },
                        // Our own ladder ran out of clock: ordinary exhaustion, reported as the
                        // transient failure that caused it.
                        None => e,
                    });
                }
                std::thread::sleep(delay);
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Backoff for the Nth retry: `BASE * 2^(attempt-1)` plus up to that much jitter.
fn backoff(attempt: u32) -> Duration {
    let base = BASE_DELAY_MS.saturating_mul(2u64.saturating_pow(attempt - 1));
    Duration::from_millis(base.saturating_add(jitter(base)))
}

/// Cheap process-local jitter in `[0, base)` without pulling in a `rand` dependency: sub-second clock
/// noise is plenty to decorrelate concurrent workers' retry storms.
fn jitter(base: u64) -> u64 {
    if base == 0 {
        return 0;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos % base
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn retries_transient_then_succeeds() {
        let calls = Cell::new(0u32);
        let out: Result<u32> = with_retry(|| {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(EngineError::Timeout { who: "test".into() })
            } else {
                Ok(42)
            }
        });
        assert_eq!(out.unwrap(), 42);
        assert_eq!(calls.get(), 3, "should retry twice then succeed");
    }

    #[test]
    fn stops_at_max_tries() {
        let calls = Cell::new(0u32);
        let out: Result<u32> = with_retry(|| {
            calls.set(calls.get() + 1);
            Err(EngineError::ServerError {
                who: "test".into(),
                status: 503,
            })
        });
        assert!(out.is_err());
        assert_eq!(calls.get(), MAX_TRIES, "should give up after MAX_TRIES");
    }

    /// The provider's stated schedule outranks the computed rung: a 60ms `Retry-After` must space
    /// the attempts by at least that, not by the 200ms first step it replaces.
    #[test]
    fn stated_wait_replaces_the_computed_rung() {
        let calls = Cell::new(0u32);
        let started = std::time::Instant::now();
        let out: Result<u32> = with_retry(|| {
            calls.set(calls.get() + 1);
            Err(EngineError::RateLimited {
                who: "test".into(),
                retry_after: Some(Duration::from_millis(60)),
            })
        });
        assert!(out.is_err());
        assert_eq!(calls.get(), MAX_TRIES);
        let spent = started.elapsed();
        assert!(spent >= Duration::from_millis(120), "waited only {spent:?}");
    }

    /// The collision rule: a stated wait that does not fit ends the ladder — no truncated sleep, no
    /// further attempt — and reports its own terminal state carrying the number that did not fit.
    #[test]
    fn stated_wait_over_budget_ends_the_ladder() {
        let calls = Cell::new(0u32);
        let out: Result<u32> = with_retry_within(Duration::from_secs(5), || {
            calls.set(calls.get() + 1);
            Err(EngineError::RateLimited {
                who: "openai".into(),
                retry_after: Some(Duration::from_secs(300)),
            })
        });
        assert_eq!(
            calls.get(),
            1,
            "must spend no attempt it was told would fail"
        );
        match out {
            Err(EngineError::OverBudgetWait {
                who,
                asked_secs,
                attempts,
                ..
            }) => {
                assert_eq!(who, "openai");
                assert_eq!(asked_secs, 300.0, "the wait that did not fit is the record");
                assert_eq!(attempts, 1);
            }
            other => panic!("expected OverBudgetWait, got {other:?}"),
        }
    }

    /// …and the state is *distinct*: a ladder that merely runs out of clock on its own computed
    /// backoff is exhaustion, and must still report the transient failure that caused it.
    #[test]
    fn computed_ladder_out_of_clock_is_exhaustion_not_over_budget_wait() {
        let out: Result<u32> = with_retry_within(Duration::from_millis(1), || {
            Err(EngineError::ServerError {
                who: "gemini".into(),
                status: 503,
            })
        });
        assert!(matches!(out, Err(EngineError::ServerError { .. })));
    }

    #[test]
    fn does_not_retry_non_transient() {
        let calls = Cell::new(0u32);
        let out: Result<u32> = with_retry(|| {
            calls.set(calls.get() + 1);
            Err(EngineError::Auth {
                who: "test".into(),
                status: 401,
            })
        });
        assert!(out.is_err());
        assert_eq!(calls.get(), 1, "auth failure is not retried");
    }
}
