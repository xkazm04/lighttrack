use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A unit of background work (Phase 3.6d). Enqueued by the API, executed by `lt-runner serve`,
/// so long operations (benchmark runs) never block ingestion. Cloud path swaps the table for Pub/Sub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    #[serde(default = "crate::new_id")]
    pub id: String,
    /// Job kind, e.g. `bench_run`.
    #[serde(rename = "type")]
    pub job_type: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
    /// `queued` | `running` | `cancelling` | `cancelled` | `done` | `failed`.
    ///
    /// `cancelling` is a *running* job an operator asked to stop: the worker notices at its next
    /// case boundary and finishes it as `cancelled`. It is deliberately outside the claimable set,
    /// so the stale-claim reclaim path can never restart a run someone cancelled.
    #[serde(default = "default_status")]
    pub status: String,
    /// How many times a worker has CLAIMED this job. Bumped inside the atomic claim, so it counts
    /// crashes too — which is why it is no longer what decides a retry (see `failures`).
    #[serde(default)]
    pub attempts: u32,
    /// How many times the job actually RAN and failed (the worker reported an error). This — not
    /// `attempts` — is the retry budget, so a worker that dies mid-run no longer burns one of the
    /// job's three chances.
    #[serde(default)]
    pub failures: u32,
    /// How many times this job was reclaimed after a worker held it past the stale window without
    /// finishing: the count of *worker deaths*, kept apart from `failures` so an operator can tell
    /// "the judge failed every case" from "the worker was killed".
    #[serde(default)]
    pub stale_reclaims: u32,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub result: Value,
    /// When the holding worker last proved it is alive — set by the claim and moved forward by
    /// every lease renewal.
    ///
    /// It is also this job's **fencing token**. A claim (or a reclaim) stamps a new value here, so
    /// a worker that holds `claimed_at = T` and finds the row no longer at `T` has learned that
    /// someone else owns the job now. Every write a worker makes about a job it believes it holds
    /// carries this value and is refused if it does not match — see [`crate::JobFinish`]. That is
    /// what stops a slow worker, reclaimed while it was busy, from overwriting the verdict its
    /// replacement already wrote.
    ///
    /// Because it now moves on renewal rather than only on claim, the staleness window is a
    /// **detection latency** (how long a dead worker may go unnoticed — minutes) and no longer has
    /// to be sized to the longest legitimate job (hours). Those are the two quantities a single
    /// claim timestamp used to conflate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<DateTime<Utc>>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
    #[serde(default = "Utc::now")]
    pub updated_at: DateTime<Utc>,
}

fn default_status() -> String {
    "queued".to_string()
}

/// What a cancel request did to a job. Returned by `Store::cancel_job` so the API can answer
/// honestly instead of pretending every cancel stopped something.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum JobCancel {
    /// It was still queued — nothing ever ran, and nothing ever will.
    Cancelled,
    /// It was running: marked `cancelling`. The worker stops at its next case boundary and finishes
    /// it as `cancelled`, keeping whatever partial results it already produced.
    Cancelling,
    /// It had already reached a terminal state (`done` / `failed` / `cancelled`); nothing changed.
    AlreadyFinished { status: String },
}

/// What a finish attempt did, so a worker learns whether its verdict landed instead of assuming it.
///
/// The unconditioned finish this replaces was the queue's last unfenced write: a worker reclaimed
/// as stale would keep running (nothing had told it otherwise), eventually finish, and overwrite
/// whatever verdict its replacement had already recorded — silently, with a plausible-looking
/// result. A completion is a transition like any other and goes through the same conditioned door:
/// *set the verdict where the job is still non-terminal and still mine*.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum JobFinish {
    /// The verdict landed.
    Finished,
    /// Refused. The caller does not hold this job any more — its lease expired and someone
    /// reclaimed it, an operator requeued it, or it already reached a terminal state. `status` and
    /// `claimed_at` are what the record says NOW, so the loser can log what beat it rather than
    /// guessing.
    NotHeld {
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        claimed_at: Option<DateTime<Utc>>,
    },
    /// There is no such job (→ 404). Distinct from `NotHeld`, which is a live job someone else owns.
    NoSuchJob,
}

/// Prefix on a stored job error that means **the worker died**, not that the benchmark failed. The
/// atomic claim writes it when it reclaims a job whose worker never finished, so the distinction
/// survives in the job row an operator actually reads.
pub const JOB_ERROR_WORKER_LOST: &str =
    "worker lost: the worker holding this job did not finish before the stale-claim timeout \
     (crashed, killed, or partitioned) — this is not a benchmark failure";

/// Prefix on a stored job error that means the job RAN and the work itself failed.
pub const JOB_ERROR_PREFIX_FAILURE: &str = "benchmark failure: ";

/// Terminal statuses: a job in one of these is never claimed or retried again.
pub fn job_is_terminal(status: &str) -> bool {
    matches!(status, "done" | "failed" | "cancelled")
}

fn default_max_attempts() -> u32 {
    3
}
