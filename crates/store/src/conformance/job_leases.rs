//! `Surface::JobLeases`: cancellation and lease renewal — the liveness half of the queue, and the
//! property that matters most about it (a cancelled run is never restarted by the reclaim path).

use chrono::Utc;
use serde_json::{json, Value};

use lighttrack_core::{new_id, JobCancel, JobFinish};

use super::jobs::{drain_jobs, new_job};
use crate::{Result, Store};

/// Cancellation, and the property that matters most about it: a cancelled run is **never restarted
/// by the stale-claim reclaim path**. A backend that can't cancel must say so (`Unsupported` → 501),
/// never quietly do nothing.
pub(super) fn job_cancellation(store: &dyn Store) -> Result<()> {
    let queued = new_job();
    store.create_job(&queued)?;
    match store.cancel_job(&queued.id) {
        Err(e) => return Err(e),
        Ok(outcome) => assert_eq!(
            outcome,
            Some(JobCancel::Cancelled),
            "a queued job is cancelled outright — nothing ran"
        ),
    }
    assert_eq!(store.get_job(&queued.id)?.expect("get").status, "cancelled");
    // Cancelling an unknown job is None (→ 404), not a fabricated success.
    assert_eq!(store.cancel_job(&new_id())?, None);
    // Cancelling something terminal reports that nothing was stopped.
    assert!(
        matches!(
            store.cancel_job(&queued.id)?,
            Some(JobCancel::AlreadyFinished { .. })
        ),
        "re-cancelling a cancelled job must not claim to have stopped it"
    );

    // A RUNNING job: cancel marks it `cancelling`, and the reclaim path must not resurrect it even
    // though its claim is (deliberately) already stale.
    drain_jobs(store)?;
    let running = new_job();
    store.create_job(&running)?;
    let claimed = store
        .claim_job(Utc::now())?
        .expect("claim the job just enqueued");
    assert_eq!(
        claimed.id, running.id,
        "the drained queue's only job is ours"
    );
    assert_eq!(store.cancel_job(&running.id)?, Some(JobCancel::Cancelling));
    assert_eq!(
        store.get_job(&running.id)?.expect("get").status,
        "cancelling"
    );
    // `Utc::now()` as the staleness cutoff makes every claim in existence stale. The cancelled job
    // must STILL not come back — this is the race the reclaim path used to lose.
    for id in drain_jobs(store)? {
        assert_ne!(
            id, running.id,
            "a cancelled run must never be reclaimed as stale"
        );
    }
    assert_eq!(
        store.get_job(&running.id)?.expect("get").status,
        "cancelling",
        "reclaim must not have touched the cancelled job"
    );
    Ok(())
}

/// **The lease invariant**, held identically by every backend: a job's holder can prove it is alive,
/// a holder that has been replaced cannot write, and a verdict is final.
///
/// This is the property whose absence let a long benchmark be stolen and then have its result
/// clobbered. Before it, `finish_job` was unconditioned in all three backends: the original worker —
/// still running, because nothing had told it otherwise — would eventually finish and overwrite the
/// verdict its replacement had already recorded, with a plausible-looking result and no error
/// anywhere. Each backend implements the conditioned write in its own dialect (SQLite `UPDATE …
/// WHERE`, Postgres the same, Firestore an `updateTime` precondition over a read-compare loop), and
/// a divergence in any of them is a silent correctness hole, so the contract is pinned here rather
/// than in one backend's unit tests.
pub(super) fn job_leases(store: &dyn Store) -> Result<()> {
    drain_jobs(store)?;
    let j = new_job();
    store.create_job(&j)?;
    let held = store.claim_job(Utc::now())?.expect("claim");
    assert_eq!(held.id, j.id, "the drained queue's only job is ours");
    let fence = held
        .claimed_at
        .expect("a claim stamps the lease it hands out");

    // ---- renewal is the liveness proof ----
    let renewed = store
        .renew_job_lease(&j.id, fence)?
        .expect("the holder's renewal must succeed");
    assert!(
        renewed >= fence,
        "renewal moves the lease forward, never back: {renewed} < {fence}"
    );
    let after = store.get_job(&j.id)?.expect("get");
    assert_eq!(
        after.claimed_at,
        Some(renewed),
        "the renewed lease is what a reaper will read"
    );
    assert_eq!(after.status, "running", "renewal is not a state change");

    // A renewal bearing the OLD fence is a lost lease, not a retry. This is the executor's own gate
    // on its legitimacy: an executor that keeps working after this returns None is a zombie whose
    // effects interleave with its successor's.
    assert_eq!(
        store.renew_job_lease(&j.id, fence)?,
        None,
        "a stale fence must not renew — that is how a zombie keeps its lease alive"
    );
    assert_eq!(
        store.renew_job_lease(&new_id(), renewed)?,
        None,
        "renewing a job that does not exist reports the loss, never a fabricated success"
    );

    // ---- the finish is fenced like every other write ----
    assert_eq!(
        store.finish_job(&j.id, "done", &json!({ "stale": true }), None, Some(fence))?,
        JobFinish::NotHeld {
            status: "running".into(),
            claimed_at: Some(renewed),
        },
        "a worker that no longer holds the job must be REFUSED, and told what holds it now"
    );
    let untouched = store.get_job(&j.id)?.expect("get");
    assert_eq!(
        untouched.status, "running",
        "the refused write changed nothing"
    );
    assert_eq!(untouched.result, Value::Null);

    // The rightful holder finishes.
    assert_eq!(
        store.finish_job(&j.id, "done", &json!({ "ok": true }), None, Some(renewed))?,
        JobFinish::Finished
    );
    assert_eq!(store.get_job(&j.id)?.expect("get").status, "done");

    // ---- a verdict is final ----
    // This is the clobber the whole mechanism exists to stop, in its purest form: the replaced
    // worker turns up late and tries to write its own answer over a terminal one.
    assert!(
        matches!(
            store.finish_job(
                &j.id,
                "failed",
                &json!({ "late": true }),
                Some("too late"),
                Some(renewed)
            )?,
            JobFinish::NotHeld { .. }
        ),
        "a terminal verdict must never be rewritten, not even by the worker that wrote it"
    );
    let final_job = store.get_job(&j.id)?.expect("get");
    assert_eq!(final_job.status, "done", "the verdict stands");
    assert_eq!(final_job.result, json!({ "ok": true }));
    assert_eq!(
        final_job.error, None,
        "and no late error was grafted onto it"
    );

    // An unfenced (operator) finish is still refused on a terminal job — `fence: None` waives the
    // ownership condition, never the finality one.
    assert!(matches!(
        store.finish_job(&j.id, "cancelled", &Value::Null, None, None)?,
        JobFinish::NotHeld { .. }
    ));
    assert_eq!(store.get_job(&j.id)?.expect("get").status, "done");

    // A dead lease cannot be renewed back to life.
    assert_eq!(
        store.renew_job_lease(&j.id, renewed)?,
        None,
        "a terminal job has no lease to extend"
    );

    // Finishing a job that does not exist is distinguishable from losing one that does — the caller
    // needs to tell "someone beat me" from "that id is wrong".
    assert_eq!(
        store.finish_job(&new_id(), "done", &Value::Null, None, None)?,
        JobFinish::NoSuchJob
    );
    Ok(())
}
