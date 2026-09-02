//! Stored recurrence: **which recurring workload, how often, and when next** — as a row.
//!
//! Before M7 recurrence had two homes and neither was one. A benchmark could carry
//! `target.schedule_interval_secs` ([`crate::RECURRENCE_KEY`]), which a sweep inside the runner
//! read — so recurrence existed only where the runner ran, applied only to benchmarks, and could
//! not attach to a *matrix* `target` at all, meaning compare benchmarks (the headline mode) simply
//! could not recur. Everything else recurred by being started with `--interval`, i.e. by a process
//! staying alive, which is not a record of anything: nothing could answer "what runs on a schedule
//! here" without reading five daemons' command lines.
//!
//! A [`Schedule`] is that answer. It names a [`JobKind`](crate::JobKind) and a payload, so any kind
//! of work recurs by the same mechanism; it carries its own `next_due`, so the sweep is a cheap
//! indexed read; and it is enumerable, so `GET /v1/schedules` lists every recurring workload.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Floor on `interval_secs`. A schedule is upkeep, not a hot loop: the sweep itself runs on a
/// coarse cadence, so anything under a minute would only ever fire as fast as the sweep anyway
/// while implying a precision the mechanism does not have.
pub const MIN_INTERVAL_SECS: u32 = 60;

/// One recurring workload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    #[serde(default = "crate::new_id")]
    pub id: String,
    pub project_id: String,
    /// The [`JobKind`](crate::JobKind) wire literal this schedule enqueues (`bench_run`, …). A
    /// `String` on the row for the same reason `Job::job_type` is: a row written by a newer version
    /// deserializes on an older one instead of hard-failing the whole list.
    pub kind: String,
    /// The job payload, enqueued verbatim. Validated against `kind` when the schedule is written.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
    pub interval_secs: u32,
    /// When the sweep should next enqueue for this schedule.
    pub next_due: DateTime<Utc>,
    /// The most recent job this schedule produced — the link `GET /v1/schedules/:id/runs` follows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_job_id: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

impl Schedule {
    /// The kind this schedule enqueues, or `None` when the row names a kind this build does not know.
    pub fn kind(&self) -> Option<crate::JobKind> {
        crate::JobKind::from_wire(&self.kind)
    }

    /// Where `next_due` lands after a sweep fires this schedule at `now`.
    ///
    /// Anchored to `now`, not to the old `next_due`. Anchoring to the previous due time is the
    /// classic cron catch-up: a sweep that was down for a day comes back and fires a day's worth of
    /// intervals at once, which for a benchmark means a day of generation spend nobody asked for.
    /// A missed window is skipped, and the operator's cadence resumes from the moment it resumed.
    pub fn advance_from(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now + Duration::seconds(self.interval_secs.max(MIN_INTERVAL_SECS) as i64)
    }
}

fn yes() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sched(interval: u32) -> Schedule {
        Schedule {
            id: "s1".into(),
            project_id: "p1".into(),
            kind: "bench_run".into(),
            payload: json!({ "benchmark_id": "b1" }),
            interval_secs: interval,
            next_due: Utc::now(),
            last_job_id: None,
            enabled: true,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn advancing_never_replays_a_missed_window() {
        let mut s = sched(3600);
        // The schedule was due a full day ago (the sweep was down). Advancing from NOW must land one
        // interval ahead — not twenty-four catch-up firings' worth behind.
        s.next_due = Utc::now() - Duration::days(1);
        let now = Utc::now();
        let next = s.advance_from(now);
        assert!(next > now, "next_due must move forward");
        assert_eq!((next - now).num_seconds(), 3600);
    }

    #[test]
    fn the_interval_floor_holds_even_on_a_row_that_predates_it() {
        // A stored row can carry anything; the floor is applied where it matters, on the next due
        // time, so a 0-second schedule cannot become a spin loop.
        let s = sched(0);
        let now = Utc::now();
        assert_eq!(
            (s.advance_from(now) - now).num_seconds(),
            MIN_INTERVAL_SECS as i64
        );
    }

    #[test]
    fn kind_reads_through_the_job_vocabulary_and_tolerates_a_stranger() {
        assert_eq!(sched(60).kind(), Some(crate::JobKind::BenchRun));
        let mut s = sched(60);
        s.kind = "from_a_newer_release".into();
        assert_eq!(
            s.kind(),
            None,
            "an unknown kind must read as None, not panic"
        );
    }
}
