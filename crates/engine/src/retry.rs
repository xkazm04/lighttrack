//! Bounded exponential-backoff retries for transient provider failures (429 / 5xx / timeout). The
//! judge is *unbudgeted*, so a rate-limited or briefly-overloaded endpoint is worth a few jittered
//! retries rather than dropping the sample. Classification is by typed [`EngineError`] variant — never
//! by string-matching provider messages.

use std::time::Duration;

use crate::{EngineError, Result};

/// Total attempts (1 initial + 2 retries).
const MAX_TRIES: u32 = 3;
/// First backoff step; doubles each retry, plus jitter.
const BASE_DELAY_MS: u64 = 200;

impl EngineError {
    /// Transient failures worth retrying: rate limits, 5xx, and timeouts. Auth/bad-request/parse
    /// failures are deterministic and are surfaced immediately.
    pub(crate) fn is_retryable(&self) -> bool {
        matches!(
            self,
            EngineError::RateLimited { .. }
                | EngineError::ServerError { .. }
                | EngineError::Timeout { .. }
        )
    }

    /// A provider that produced no completion text (distinct from output that failed to parse).
    pub(crate) fn is_empty_completion(&self) -> bool {
        matches!(self, EngineError::EmptyCompletion { .. })
    }
}

/// Run `f`, retrying transient failures with bounded, jittered exponential backoff. Non-retryable
/// errors (and successes) return immediately.
pub(crate) fn with_retry<T>(mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let mut attempt = 1;
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if e.is_retryable() && attempt < MAX_TRIES => {
                std::thread::sleep(backoff(attempt));
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
            Err(EngineError::ServerError { who: "test".into(), status: 503 })
        });
        assert!(out.is_err());
        assert_eq!(calls.get(), MAX_TRIES, "should give up after MAX_TRIES");
    }

    #[test]
    fn does_not_retry_non_transient() {
        let calls = Cell::new(0u32);
        let out: Result<u32> = with_retry(|| {
            calls.set(calls.get() + 1);
            Err(EngineError::Auth { who: "test".into(), status: 401 })
        });
        assert!(out.is_err());
        assert_eq!(calls.get(), 1, "auth failure is not retried");
    }
}
