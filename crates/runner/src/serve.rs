//! `serve`: the job-queue worker loop — claim a job, run it (watching for cancellation and
//! publishing live progress), finish it, and retry only what actually failed.

use std::time::Duration;

use anyhow::Result;
use lighttrack_engine::EngineConfig;

use crate::cli::Cli;
use crate::serve_api::claim;
use crate::serve_job::run_claimed_job;
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
                    // A zero poll interval is a claim storm against the API; `--once` is the
                    // spelling for "do not wait".
                    std::thread::sleep(Duration::from_secs(interval.max(1)));
                }
            }
        }
        if once {
            break;
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::{declared, providers_from_env, renew_every};
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
}
