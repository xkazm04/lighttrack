//! Running ONE claimed job: the cancel watcher, the heartbeat, and the honest finish.
//!
//! Split from the claim loop because this is where the lease actually lives. Two threads run beside
//! the work — one asking whether an operator cancelled it, one proving this worker is still alive —
//! and both exist to answer a question the work itself cannot: is this run still legitimate?

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;

use lighttrack_core::{Job, JOB_ERROR_PREFIX_FAILURE};
use lighttrack_engine::EngineConfig;

use crate::cli::Cli;
use crate::dispatch::process_job;
use crate::runctl::{RunControl, CANCEL_POLL_INTERVAL};
use crate::serve_api::{finish, renew_lease};

/// Execute one claimed job with a cancel watcher alongside it, then finish it honestly.
///
/// The retry decision uses `failures` — runs that actually failed — not `attempts`, which the claim
/// bumps for a crashed worker too. Three crashes used to permanently fail a job and record the crash
/// as its error, hiding whether the benchmark had ever failed at all.
pub(crate) fn run_claimed_job(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    job: &Job,
    renew: Duration,
) -> Result<()> {
    let ctl = RunControl::for_job(cli, http, &job.id);
    ctl.note("starting");
    let finished = AtomicBool::new(false);
    // The lease this worker holds. The claim handed it over; the heartbeat moves it forward; and
    // every write this worker makes about the job carries the current value as its fencing token.
    let lease = Lease::new(job.claimed_at);

    let outcome = std::thread::scope(|scope| {
        // Watcher: ask the API whether an operator cancelled this job. The run itself notices at its
        // next case boundary — nothing is ever interrupted mid-call.
        scope.spawn(|| {
            while !finished.load(Ordering::Relaxed) {
                std::thread::sleep(CANCEL_POLL_INTERVAL);
                if finished.load(Ordering::Relaxed) {
                    break;
                }
                if ctl.poll_cancelled() {
                    eprintln!("  cancel requested — stopping at the next case boundary");
                    ctl.cancel();
                    break;
                }
            }
        });
        // Heartbeat: prove this worker is alive, on a TIMER — never per case. A renewal loop driven
        // by units of work silently stops renewing inside the one step that takes an hour, which is
        // exactly the step during which the lease matters. It also carries no progress: liveness
        // must never wait on anything the work computes, or a live-but-stuck worker reads as dead.
        scope.spawn(|| {
            while !finished.load(Ordering::Relaxed) {
                std::thread::sleep(renew);
                if finished.load(Ordering::Relaxed) {
                    break;
                }
                match renew_lease(cli, http, &job.id, lease.get()) {
                    Ok(Some(next)) => lease.set(next),
                    Ok(None) => {
                        // The lease is gone: a reaper expired it, an operator requeued the job, or
                        // someone reclaimed it. A gate whose result nobody reads gates nothing —
                        // this one stops the run. Carrying on would make this a zombie whose spend
                        // and effects interleave with its successor's.
                        eprintln!(
                            "  LEASE LOST - this job is no longer ours; stopping at the next case \
                             boundary. Nothing this run writes from here will be accepted."
                        );
                        ctl.cancel();
                        break;
                    }
                    // A transient failure is not evidence of a lost lease, and treating it as one
                    // would abandon a healthy run on a blip. That is what the TTL/3 cadence buys:
                    // room to miss one or two and try again.
                    Err(e) => eprintln!("  lease renewal failed (will retry): {e}"),
                }
            }
        });
        let outcome = process_job(cli, http, engine, job, &ctl);
        finished.store(true, Ordering::Relaxed);
        outcome
    });

    match outcome {
        Ok(result) => {
            // A cancelled run's partial results are already recorded (and marked partial) by the
            // benchmark itself; the job says so too, and carries no error — cancelling is not a
            // failure, so it must not consume a retry.
            let status = if ctl.cancelled() { "cancelled" } else { "done" };
            report(
                finish(cli, http, &job.id, status, &result, None, lease.get()),
                status,
            );
        }
        Err(e) => {
            let (status, note) = retry_decision(job.failures, job.max_attempts);
            let error = format!("{JOB_ERROR_PREFIX_FAILURE}{e}");
            report(
                finish(
                    cli,
                    http,
                    &job.id,
                    status,
                    &Value::Null,
                    Some(&error),
                    lease.get(),
                ),
                &format!("{status} ({note}): {e}"),
            );
        }
    }
    Ok(())
}

/// Say what the finish did. A refusal (HTTP 409 - the lease moved, or the job is already terminal)
/// is LOUD but not fatal: this worker lost a race it was always going to lose, the verdict that
/// stands belongs to whoever holds the job now, and the loop must go back to claiming rather than
/// dying. Swallowing it silently would be worse than the clobber this replaced, because nobody
/// would ever learn that a run's result went nowhere.
fn report(outcome: Result<()>, what: &str) {
    match outcome {
        Ok(()) => println!("  -> {what}"),
        Err(e) => eprintln!(
            "  -> VERDICT NOT RECORDED ({what}): {e}\n     This worker no longer held the job; \
             whoever holds it now owns the outcome."
        ),
    }
}

/// The lease a worker holds, shared between the heartbeat thread and the finish.
///
/// A fence is an identity, not a moment: what matters is that the stamp still matches exactly.
struct Lease(std::sync::Mutex<Option<DateTime<Utc>>>);

impl Lease {
    fn new(initial: Option<DateTime<Utc>>) -> Self {
        Lease(std::sync::Mutex::new(initial))
    }
    fn get(&self) -> Option<DateTime<Utc>> {
        self.0.lock().ok().and_then(|g| *g)
    }
    fn set(&self, v: DateTime<Utc>) {
        if let Ok(mut g) = self.0.lock() {
            *g = Some(v);
        }
    }
}

/// Whether a *reported* failure retries, given how many real failures the job has already had.
/// Returns the next status and a human note. Pure, so the accounting is testable.
fn retry_decision(failures: u32, max_attempts: u32) -> (&'static str, String) {
    // `failures` is the count BEFORE this one; finishing with an error records it.
    let after = failures + 1;
    if after < max_attempts {
        (
            "queued",
            format!("failure {after}/{max_attempts}, retrying"),
        )
    } else {
        (
            "failed",
            format!("failure {after}/{max_attempts}, giving up"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::retry_decision;

    #[test]
    fn only_reported_failures_consume_the_retry_budget() {
        // First real failure of three → retry.
        assert_eq!(retry_decision(0, 3).0, "queued");
        assert_eq!(retry_decision(1, 3).0, "queued");
        // Third → give up, and say which failure it was.
        let (status, note) = retry_decision(2, 3);
        assert_eq!(status, "failed");
        assert!(note.contains("3/3"), "{note}");
        // The regression this replaces: a job whose worker was killed twice (attempts=3) but which
        // never actually failed still gets its full retry budget, because `failures` is what counts.
        assert_eq!(retry_decision(0, 3).0, "queued");
    }
}
