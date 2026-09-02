//! `serve`: the job-queue worker loop — claim a job, run it (watching for cancellation and
//! publishing live progress), finish it, and retry only what actually failed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use lighttrack_core::{Job, JOB_ERROR_PREFIX_FAILURE};
use lighttrack_engine::EngineConfig;

use crate::cli::Cli;
use crate::dispatch::process_job;
use crate::http::post;
use crate::runctl::{RunControl, CANCEL_POLL_INTERVAL};
use crate::util::short;

/// How often the holder proves it is alive, given the lease TTL. A third is the conventional
/// fraction and the reason is arithmetic: at TTL/3 a worker can miss two consecutive renewals — a
/// GC pause, a transient API error, a slow round trip — and still hold its job. A heartbeat at the
/// TTL itself converts every hiccup into a spurious takeover.
fn renew_every(stale_secs: i64, override_secs: u64) -> Duration {
    if override_secs > 0 {
        return Duration::from_secs(override_secs);
    }
    Duration::from_secs((stale_secs.max(3) as u64 / 3).max(1))
}

/// Everything one `serve` invocation needs (a struct rather than eight positional arguments).
pub(crate) struct ServeParams {
    pub once: bool,
    pub interval: u64,
    pub stale_secs: i64,
    pub lease_renew_secs: u64,
    /// The job kinds this worker will claim. Empty = all of them.
    pub kinds: Vec<String>,
    /// Which model providers this worker holds credentials for. Declared to the API, which records
    /// it so an operator can see why a queue is not draining.
    pub providers: Vec<String>,
}

pub(crate) fn serve(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    p: &ServeParams,
) -> Result<()> {
    let (once, interval, stale_secs) = (p.once, p.interval, p.stale_secs);
    let renew = renew_every(stale_secs, p.lease_renew_secs);
    // Ask once whether the local Claude CLI can actually run, before this worker starts claiming
    // jobs that may need it. Unlike the responder, `serve` does NOT exit: most job types never
    // touch the CLI (Gemini/OpenAI judging, deterministic rubrics), so a missing install disables a
    // subset of the queue rather than justifying refusing all of it.
    let probe = lighttrack_engine::probe(&engine.claude_bin);
    if probe.installed {
        println!("lt-runner serve: {}", probe.summary());
    } else {
        eprintln!(
            "lt-runner serve: {} — jobs that need `claude -p` will fail; provider-API judging is \
             unaffected",
            probe.summary()
        );
    }
    println!(
        "lt-runner serve: polling {} (interval={interval}s, once={once}, \
         kinds={}, providers={}, lease={stale_secs}s renewed every {}s)",
        cli.base,
        declared(&p.kinds),
        declared(&p.providers),
        renew.as_secs()
    );
    // Recurrence is no longer this loop's business: it is a stored `Schedule` swept by the API,
    // which is the process that is always deployed. A worker that also swept would silently be the
    // only source of recurrence in a deployment that happens to run one.
    loop {
        match claim(cli, http, stale_secs, &p.kinds, &p.providers)? {
            Some(job) => {
                println!(
                    "claimed job {} type={} (attempt {}/{}, failures {}, worker deaths {})",
                    short(&job.id),
                    job.job_type,
                    job.attempts,
                    job.max_attempts,
                    job.failures,
                    job.stale_reclaims,
                );
                run_claimed_job(cli, http, engine, &job, renew)?;
            }
            None => {
                if !once {
                    std::thread::sleep(Duration::from_secs(interval));
                }
            }
        }
        if once {
            break;
        }
    }
    Ok(())
}

/// Execute one claimed job with a cancel watcher alongside it, then finish it honestly.
///
/// The retry decision uses `failures` — runs that actually failed — not `attempts`, which the claim
/// bumps for a crashed worker too. Three crashes used to permanently fail a job and record the crash
/// as its error, hiding whether the benchmark had ever failed at all.
fn run_claimed_job(
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

/// A declaration for the banner: what the worker said it can do, or "all".
fn declared(v: &[String]) -> String {
    if v.is_empty() {
        "all".to_string()
    } else {
        v.join(",")
    }
}

/// Which providers this worker can actually reach, derived from the API keys present in its
/// environment. A worker that declares nothing it holds credentials for is not a worker that can
/// judge, and the operator staring at a queue that will not drain deserves that in the claim.
pub(crate) fn providers_from_env() -> Vec<String> {
    [
        ("ANTHROPIC_API_KEY", "anthropic"),
        ("OPENAI_API_KEY", "openai"),
        ("GEMINI_API_KEY", "google"),
        ("GOOGLE_API_KEY", "google"),
    ]
    .into_iter()
    .filter(|(env, _)| std::env::var(env).is_ok_and(|v| !v.is_empty()))
    .map(|(_, name)| name.to_string())
    .fold(Vec::new(), |mut acc, p| {
        if !acc.contains(&p) {
            acc.push(p);
        }
        acc
    })
}

fn claim(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    stale_secs: i64,
    kinds: &[String],
    providers: &[String],
) -> Result<Option<Job>> {
    let v = post(
        cli,
        http,
        "/v1/jobs/claim",
        &json!({ "stale_secs": stale_secs, "kinds": kinds, "providers": providers }),
    )?;
    if v.is_null() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_value(v)?))
    }
}

/// Extend this worker's lease, returning the new one. `Ok(None)` means the lease is no longer ours
/// (the API answered 409) - affirmative evidence of a takeover, which is why it is a distinct value
/// from `Err`, i.e. "I could not tell".
fn renew_lease(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    id: &str,
    fence: Option<DateTime<Utc>>,
) -> Result<Option<DateTime<Utc>>> {
    // No lease was stamped at claim, so there is nothing to prove and nothing to lose.
    let Some(fence) = fence else {
        return Ok(None);
    };
    match post(
        cli,
        http,
        &format!("/v1/jobs/{id}/renew"),
        &json!({ "claimed_at": fence }),
    ) {
        Ok(v) => Ok(v
            .get("claimed_at")
            .and_then(|c| serde_json::from_value::<DateTime<Utc>>(c.clone()).ok())),
        Err(e) if is_conflict(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether an API error is the 409 that means "you do not hold this any more".
fn is_conflict(e: &anyhow::Error) -> bool {
    e.to_string().contains("409")
}

/// Write the verdict, FENCED on the lease this worker holds. The API refuses with 409 if the job
/// moved on - which is precisely the write that used to be unconditioned, letting a worker that had
/// already been reclaimed overwrite its replacement's verdict.
#[allow(clippy::too_many_arguments)]
fn finish(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    id: &str,
    status: &str,
    result: &Value,
    error: Option<&str>,
    fence: Option<DateTime<Utc>>,
) -> Result<()> {
    post(
        cli,
        http,
        &format!("/v1/jobs/{id}/finish"),
        &json!({ "status": status, "result": result, "error": error, "claimed_at": fence }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{declared, providers_from_env, renew_every, retry_decision};
    use std::time::Duration;

    #[test]
    fn an_undeclared_worker_still_reads_as_a_worker() {
        // Empty means "any kind" on the wire and "all" in the banner — the pre-M7 meaning, which an
        // older runner still sends.
        assert_eq!(declared(&[]), "all");
        assert_eq!(declared(&["bench_run".to_string()]), "bench_run");
    }

    #[test]
    fn provider_capabilities_come_from_the_keys_that_are_actually_present() {
        // Two env vars name the same provider; the declaration must not say it twice.
        std::env::set_var("GEMINI_API_KEY", "x");
        std::env::set_var("GOOGLE_API_KEY", "y");
        std::env::remove_var("ANTHROPIC_API_KEY");
        let p = providers_from_env();
        assert_eq!(p.iter().filter(|x| *x == "google").count(), 1);
        assert!(!p.contains(&"anthropic".to_string()));
        // An empty value is not a credential.
        std::env::set_var("GEMINI_API_KEY", "");
        std::env::remove_var("GOOGLE_API_KEY");
        assert!(!providers_from_env().contains(&"google".to_string()));
        std::env::remove_var("GEMINI_API_KEY");
    }

    #[test]
    fn the_heartbeat_leaves_room_to_miss_a_couple() {
        // A third of the TTL, so two consecutive misses still hold the lease. A cadence at (or near)
        // the TTL turns every GC pause into a spurious takeover - the mistake this encodes against.
        assert_eq!(renew_every(120, 0), Duration::from_secs(40));
        assert_eq!(renew_every(600, 0), Duration::from_secs(200));
        // An explicit override wins, for operators who know their own latency profile.
        assert_eq!(renew_every(120, 5), Duration::from_secs(5));
        // A nonsensically small TTL still yields a positive cadence rather than a busy loop.
        assert!(renew_every(1, 0) >= Duration::from_secs(1));
        assert!(renew_every(0, 0) >= Duration::from_secs(1));
    }

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
