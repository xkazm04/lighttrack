//! The job queue's core lifecycle: create, claim, progress, finish, and failure accounting.

use chrono::Utc;
use serde_json::{json, Value};

use lighttrack_core::{new_id, Job};

use super::job_leases::{job_cancellation, job_leases};

use crate::{Result, Store};

pub(super) fn new_job() -> Job {
    let now = Utc::now();
    Job {
        id: new_id(),
        job_type: "conf".into(),
        payload: json!({ "k": "v" }),
        status: "queued".into(),
        attempts: 0,
        max_attempts: 3,
        failures: 0,
        stale_reclaims: 0,
        progress: None,
        error: None,
        result: Value::Null,
        claimed_at: None,
        created_at: now,
        updated_at: now,
    }
}

pub(super) fn jobs(store: &dyn Store) -> Result<()> {
    let now = Utc::now();
    let j = new_job();
    store.create_job(&j)?;
    assert_eq!(
        store.get_job(&j.id)?.expect("get_job Some").status,
        "queued"
    );

    // Claim is global (oldest queued/stale first), so on a shared DB it may return another job —
    // assert only that a job was claimed and flipped to running with a bumped attempt count.
    let claimed = store.claim_job(now)?.expect("claim_job returns a job");
    assert_eq!(claimed.status, "running");
    assert!(claimed.attempts >= 1, "claim bumps attempts");

    // Our specific job's lifecycle by id (independent of which job claim() returned).
    store.update_job_progress(&j.id, "50%")?;
    store.finish_job(&j.id, "done", &json!({ "ok": true }), None, None)?;
    let done = store.get_job(&j.id)?.expect("get_job after finish");
    assert_eq!(done.status, "done");
    assert_eq!(done.result, json!({ "ok": true }), "job result round-trip");
    assert!(store
        .list_jobs(Some("done"), 100)?
        .iter()
        .any(|x| x.id == j.id));
    job_cancellation(store)?;
    job_failure_accounting(store)?;
    job_leases(store)?;
    Ok(())
}

/// Claim until the queue is empty (bounded), returning every id claimed. Lets the cancellation
/// checks below reason about a queue whose head they control, on a store whose claim is global.
pub(super) fn drain_jobs(store: &dyn Store) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for _ in 0..50 {
        match store.claim_job(Utc::now())? {
            Some(j) => ids.push(j.id),
            None => break,
        }
    }
    Ok(ids)
}

/// A worker that dies is not a benchmark that failed. `attempts` counts claims (crashes included),
/// `stale_reclaims` counts worker deaths, and `failures` — the retry budget — counts only runs that
/// actually reported an error.
fn job_failure_accounting(store: &dyn Store) -> Result<()> {
    drain_jobs(store)?;
    let j = new_job();
    store.create_job(&j)?;
    let first = store.claim_job(Utc::now())?.expect("claim");
    assert_eq!(first.id, j.id);
    assert_eq!(
        (first.attempts, first.failures, first.stale_reclaims),
        (1, 0, 0)
    );

    // Simulate the worker being killed: never finish, let the claim go stale, reclaim it.
    let second = store.claim_job(Utc::now())?.expect("reclaim the stale job");
    assert_eq!(second.id, j.id);
    assert_eq!(second.attempts, 2, "a claim is a claim, crash or not");
    assert_eq!(second.failures, 0, "a dead worker must not burn a retry");
    assert_eq!(
        second.stale_reclaims, 1,
        "…it is counted as a worker death instead"
    );
    assert!(
        second
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("worker lost"),
        "the stored error must say the worker died, not invent a benchmark failure: {:?}",
        second.error
    );

    // Now the benchmark itself fails: that IS a retry.
    store.finish_job(
        &j.id,
        "queued",
        &Value::Null,
        Some("benchmark failure: judge failed"),
        second.claimed_at,
    )?;
    let after = store.get_job(&j.id)?.expect("get");
    assert_eq!(
        after.failures, 1,
        "a reported error consumes the retry budget"
    );
    assert_eq!(
        after.stale_reclaims, 1,
        "…and is not confused with a worker death"
    );
    assert!(after
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("judge failed"));

    // A clean finish never consumes the budget. (Unfenced: the job went back to `queued` above, so
    // nobody holds it — this is the operator-shaped finish.)
    store.finish_job(&j.id, "done", &json!({ "ok": true }), None, None)?;
    assert_eq!(store.get_job(&j.id)?.expect("get").failures, 1);
    Ok(())
}
