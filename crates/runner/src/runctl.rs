//! Run control for a benchmark in flight: the **cancel** signal a queued run watches for, and the
//! **live progress** it publishes back to its job.
//!
//! Both exist because a paid, long-running benchmark used to be opaque and unstoppable: the job's
//! progress string was written exactly once at claim time ("running benchmark <id>") and stayed
//! there for 500 cases, and there was no way to stop a runaway except waiting out the stale-claim
//! window while it spent. A run stops at a **case boundary** — never mid-LLM-call — and keeps
//! whatever it already produced, marked partial.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::cli::Cli;
use crate::http::{get, post};

/// Minimum gap between progress posts. Per-case posting on a 500-case run would be 500 writes to
/// the same row; the counter is exact regardless, only its publication is throttled.
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_secs(2);

/// How often the watcher asks the API whether its job was cancelled.
pub(crate) const CANCEL_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Cancel signal + progress sink for one benchmark run. A CLI-driven run gets [`RunControl::inert`]
/// (nothing to cancel, nowhere to report); a queued run gets [`RunControl::for_job`].
pub(crate) struct RunControl<'a> {
    cancelled: AtomicBool,
    completed: AtomicUsize,
    started: Instant,
    job: Option<JobSink<'a>>,
    last_post: Mutex<Option<Instant>>,
}

struct JobSink<'a> {
    cli: &'a Cli,
    http: &'a reqwest::blocking::Client,
    job_id: String,
}

impl<'a> RunControl<'a> {
    /// A control that never cancels and reports nowhere — the direct `lt-runner bench` path.
    pub(crate) fn inert() -> Self {
        RunControl {
            cancelled: AtomicBool::new(false),
            completed: AtomicUsize::new(0),
            started: Instant::now(),
            job: None,
            last_post: Mutex::new(None),
        }
    }

    pub(crate) fn for_job(cli: &'a Cli, http: &'a reqwest::blocking::Client, job_id: &str) -> Self {
        RunControl {
            job: Some(JobSink {
                cli,
                http,
                job_id: job_id.to_string(),
            }),
            ..RunControl::inert()
        }
    }

    /// Whether the operator asked this run to stop. Checked at case boundaries, before spending.
    pub(crate) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Raise the cancel flag (called by the watcher thread, or by a signal handler).
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Record one finished case and publish progress if the throttle allows. Safe to call from the
    /// parallel workers: the counter is atomic and the post is behind a mutex.
    pub(crate) fn tick(&self, total: usize) {
        let done = self.completed.fetch_add(1, Ordering::Relaxed) + 1;
        let Some(sink) = &self.job else {
            return;
        };
        let due = {
            let Ok(mut last) = self.last_post.lock() else {
                return;
            };
            let now = Instant::now();
            let due = last.is_none_or(|t| now.duration_since(t) >= PROGRESS_MIN_INTERVAL)
                || done == total;
            if due {
                *last = Some(now);
            }
            due
        };
        if due {
            self.publish(sink, &progress_line(done, total, self.started.elapsed()));
        }
    }

    /// Publish an arbitrary progress line (run start, cancellation notice).
    pub(crate) fn note(&self, text: &str) {
        if let Some(sink) = &self.job {
            self.publish(sink, text);
        }
    }

    fn publish(&self, sink: &JobSink<'a>, text: &str) {
        // Best-effort: progress is telemetry. A failed post must never abort a paid run.
        let _ = post(
            sink.cli,
            sink.http,
            &format!("/v1/jobs/{}/progress", sink.job_id),
            &json!({ "progress": text }),
        );
    }

    /// Ask the API whether this run's job has been cancelled. `false` on any error — a transient API
    /// blip must not look like a cancellation and silently truncate a run.
    pub(crate) fn poll_cancelled(&self) -> bool {
        let Some(sink) = &self.job else {
            return false;
        };
        match get::<serde_json::Value>(sink.cli, sink.http, &format!("/v1/jobs/{}", sink.job_id)) {
            Ok(v) => matches!(
                v.get("status").and_then(serde_json::Value::as_str),
                Some("cancelling") | Some("cancelled")
            ),
            Err(_) => false,
        }
    }
}

/// The progress string, with an ETA extrapolated from the cases already judged. Pure, so the wording
/// and the ETA arithmetic are testable without a live run.
pub(crate) fn progress_line(done: usize, total: usize, elapsed: Duration) -> String {
    if total == 0 {
        return format!("{done} case(s) judged");
    }
    let pct = (done.min(total) * 100) / total;
    let eta = if done > 0 && done < total {
        let per_case = elapsed.as_secs_f64() / done as f64;
        format!(
            ", eta ~{}s",
            ((total - done) as f64 * per_case).round() as u64
        )
    } else {
        String::new()
    };
    format!("{done}/{total} cases ({pct}%){eta}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_reports_position_and_extrapolates_an_eta() {
        // 10 of 100 cases in 20s ⇒ 2s/case ⇒ ~180s left.
        let s = progress_line(10, 100, Duration::from_secs(20));
        assert_eq!(s, "10/100 cases (10%), eta ~180s");
        // The last case has nothing left to wait for.
        assert_eq!(
            progress_line(100, 100, Duration::from_secs(200)),
            "100/100 cases (100%)"
        );
        // Nothing judged yet ⇒ no ETA invented from a zero denominator.
        assert_eq!(
            progress_line(0, 50, Duration::from_secs(5)),
            "0/50 cases (0%)"
        );
        // An unknown case count still reports something true.
        assert_eq!(
            progress_line(3, 0, Duration::from_secs(1)),
            "3 case(s) judged"
        );
    }

    #[test]
    fn an_inert_control_never_cancels_and_still_counts() {
        let ctl = RunControl::inert();
        assert!(!ctl.cancelled());
        assert!(!ctl.poll_cancelled(), "no job ⇒ nothing can cancel it");
        ctl.tick(3);
        ctl.tick(3);
        assert_eq!(ctl.completed.load(Ordering::Relaxed), 2);
        ctl.cancel();
        assert!(ctl.cancelled());
    }

    #[test]
    fn ticks_are_safe_from_parallel_workers() {
        let ctl = RunControl::inert();
        std::thread::scope(|s| {
            for _ in 0..16 {
                s.spawn(|| ctl.tick(16));
            }
        });
        assert_eq!(ctl.completed.load(Ordering::Relaxed), 16, "no lost updates");
    }
}
