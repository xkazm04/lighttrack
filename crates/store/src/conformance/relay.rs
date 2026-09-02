//! `Surface::Relay`: the cloud→device task queue (docs/RELAY.md), including the fenced lease.

use chrono::Utc;
use serde_json::{json, Value};

use lighttrack_core::{new_id, RelayOutcome, RelaySettle, RelayTask};

use crate::{Result, Store};

use super::relay_lease::{relay_cancellation, relay_fencing};
use crate::Scope;

/// A queued task with an immediate retry interval, so a failed attempt becomes due again at once.
/// Shared with the refusal probe so both describe the same shape.
pub(super) fn sample_task(pid: &str, max_attempts: u32) -> RelayTask {
    let now = Utc::now();
    RelayTask {
        id: new_id(),
        project_id: pid.into(),
        source: Some("conformance".into()),
        action_type: "conf/echo".into(),
        payload: json!({ "k": "v" }),
        status: "queued".into(),
        attempts: 0,
        failures: 0,
        stale_reclaims: 0,
        lease_fence: None,
        progress: None,
        max_attempts,
        retry_interval_secs: 0, // failed attempts become due again immediately
        idempotency_key: None,
        device: None,
        lease_deadline: None,
        next_attempt_at: now,
        result: Value::Null,
        error: None,
        created_at: now,
        updated_at: now,
    }
}

/// The settled row, or a panic naming what refused instead. Every case below settles a task it
/// genuinely holds, so anything else is the failure, not a branch to handle.
pub(super) fn settled(v: RelaySettle, what: &str) -> RelayTask {
    match v {
        RelaySettle::Settled(t) => *t,
        other => panic!("{what}: expected the settle to land, got {other:?}"),
    }
}

/// Lease for this device and pick out our own task — lease is global (oldest-due first), so on a
/// shared DB the batch may carry other rows.
pub(super) fn leased_ours(store: &dyn Store, id: &str, secs: i64) -> Result<Option<RelayTask>> {
    Ok(store
        .lease_relay_tasks("conf-dev", &[], secs, 20)?
        .into_iter()
        .find(|t| t.id == id))
}

/// Relay queue (docs/RELAY.md): enqueue → lease → settle round-trips, retry/deferral accounting,
/// and the dead-letter sweep. Like the job claim, lease/sweep are global (oldest-due first), so on a
/// shared DB we assert on our ids and tolerate other rows in the results.
pub(super) fn relay(store: &dyn Store, pid: &str) -> Result<()> {
    let mut t = sample_task(pid, 2);
    t.idempotency_key = Some(new_id());
    store.create_relay_task(&t)?;

    // Round-trip + idempotency lookup.
    let got = store
        .get_relay_task(Scope::Operator, &t.id)?
        .expect("get_relay_task Some");
    assert_eq!(got.payload, json!({ "k": "v" }), "relay payload round-trip");
    let key = t.idempotency_key.clone().unwrap();
    assert_eq!(
        store.find_relay_task_by_key(pid, &key)?.expect("by key").id,
        t.id
    );
    assert!(store
        .find_relay_task_by_key("other-project", &key)?
        .is_none());

    // A lease stamps a fence and consumes a CLAIM (not a retry); a reported failure requeues (zero
    // interval ⇒ due again) with the error, and consumes one of the retry budget instead.
    let leased = leased_ours(store, &t.id, 60)?.expect("our task leased");
    assert_eq!(leased.status, "leased");
    assert_eq!(leased.attempts, 1);
    assert_eq!(leased.failures, 0, "leasing is not failing");
    let fence = leased.lease_fence.expect("a lease stamps its fence");
    let requeued = settled(
        store.settle_relay_task(
            &t.id,
            Some(fence),
            &RelayOutcome::Failed("conf boom".into()),
        )?,
        "failed settle",
    );
    assert_eq!(requeued.status, "queued");
    assert_eq!(requeued.error.as_deref(), Some("conf boom"));
    assert_eq!(
        requeued.failures, 1,
        "a reported failure is the retry budget"
    );
    assert!(
        requeued.lease_fence.is_none(),
        "settling releases the fence, so the next lease mints a fresh one"
    );

    // A deferral hands the consumed claim back and records no failure: the subscription window
    // being closed is not the action failing.
    let re = leased_ours(store, &t.id, 60)?.expect("re-leased");
    assert_eq!(re.attempts, 2);
    let deferred = settled(
        store.settle_relay_task(
            &t.id,
            re.lease_fence,
            &RelayOutcome::Deferred {
                retry_after_secs: Some(0),
                reason: Some("window".into()),
            },
        )?,
        "deferred settle",
    );
    assert_eq!(deferred.status, "queued");
    assert_eq!(deferred.attempts, 1, "deferral hands the claim back");
    assert_eq!(deferred.failures, 1, "…and records no new failure");

    // Success is terminal; a duplicate report is refused as NotHeld rather than re-applied.
    let held = leased_ours(store, &t.id, 60)?.expect("leased again");
    let done = settled(
        store.settle_relay_task(
            &t.id,
            held.lease_fence,
            &RelayOutcome::Succeeded(json!({ "ok": true })),
        )?,
        "success settle",
    );
    assert_eq!(done.status, "succeeded");
    assert_eq!(
        done.result,
        json!({ "ok": true }),
        "relay result round-trip"
    );
    assert!(
        matches!(
            store.settle_relay_task(
                &t.id,
                held.lease_fence,
                &RelayOutcome::Failed("late".into())
            )?,
            RelaySettle::NotHeld { .. }
        ),
        "a duplicate report must not re-open or overwrite a settled task"
    );
    assert_eq!(
        store
            .get_relay_task(Scope::Operator, &t.id)?
            .expect("get")
            .status,
        "succeeded",
        "…and must leave the terminal verdict exactly as it was"
    );
    assert!(store
        .list_relay_tasks(Scope::Project(pid), Some("succeeded"), 100)?
        .iter()
        .any(|x| x.id == t.id));
    // Narrowed to one action (M19): the snapshot behind `POST /v1/relay/actions/:t/dataset` must
    // return this action's runs and nobody else's. A filter that quietly ignored `action_type`
    // would build a dataset out of a neighbouring action's traffic and look perfectly healthy.
    assert!(store
        .list_relay_tasks_by_action(Scope::Project(pid), &t.action_type, Some("succeeded"), 100)?
        .iter()
        .any(|x| x.id == t.id));
    assert!(
        store
            .list_relay_tasks_by_action(Scope::Project(pid), "conf/not-an-action", None, 100)?
            .is_empty(),
        "an action_type nothing was enqueued under must return nothing, not everything"
    );
    assert!(matches!(
        store.settle_relay_task(&new_id(), None, &RelayOutcome::Failed("x".into()))?,
        RelaySettle::NoSuchTask
    ));

    // An exhausted RETRY budget dead-letters…
    let doomed = sample_task(pid, 1);
    store.create_relay_task(&doomed)?;
    let held = leased_ours(store, &doomed.id, 60)?.expect("doomed leased");
    let dead = settled(
        store.settle_relay_task(
            &doomed.id,
            held.lease_fence,
            &RelayOutcome::Failed("final".into()),
        )?,
        "final settle",
    );
    assert_eq!(dead.status, "dead");
    assert_eq!(dead.failures, 1);

    relay_dead_sweep(store, pid)?;
    relay_fencing(store, pid)?;
    relay_cancellation(store, pid)?;
    Ok(())
}

/// A device that vanishes is not an action that failed. The sweep dead-letters an expired lease
/// only once a budget is genuinely gone — and a task with retries left goes back to a device
/// instead, carrying a `stale_reclaims` count that says what actually happened.
fn relay_dead_sweep(store: &dyn Store, pid: &str) -> Result<()> {
    // Retries to spare: the expired lease is RECLAIMED, counted as a device death, and not killed.
    let survivor = sample_task(pid, 3);
    store.create_relay_task(&survivor)?;
    leased_ours(store, &survivor.id, 0)?.expect("survivor leased");
    let swept = store.sweep_relay_dead()?;
    assert!(
        !swept.iter().any(|x| x.id == survivor.id),
        "a device death must not dead-letter a task that still has retries"
    );
    let again = leased_ours(store, &survivor.id, 60)?.expect("reclaimed after the lease expired");
    assert_eq!(again.stale_reclaims, 1, "…it is counted as a device death");
    assert_eq!(again.failures, 0, "…and never as an action failure");
    assert!(
        again
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("device lost"),
        "the stored error must say the device died: {:?}",
        again.error
    );

    // Device deaths have their own budget, and it is what the sweep enforces. A task nothing ever
    // reports on cannot exhaust the RETRY budget — no device lives long enough to report a failure —
    // so a single counter would re-lease it forever. Keep killing the device until that budget is
    // gone, and the sweep is the thing that finally dead-letters it.
    let cursed = sample_task(pid, 9);
    store.create_relay_task(&cursed)?;
    for _ in 0..=lighttrack_core::RELAY_MAX_STALE_RECLAIMS {
        leased_ours(store, &cursed.id, 0)?; // zero-second lease: expires at once
    }
    let held = store
        .get_relay_task(Scope::Operator, &cursed.id)?
        .expect("get cursed");
    assert_eq!(held.status, "leased");
    assert_eq!(
        held.stale_reclaims,
        lighttrack_core::RELAY_MAX_STALE_RECLAIMS
    );
    assert!(
        leased_ours(store, &cursed.id, 0)?.is_none(),
        "a task that has killed its device budget must not be leased again"
    );
    let swept = store.sweep_relay_dead()?;
    let ours = swept
        .iter()
        .find(|x| x.id == cursed.id)
        .expect("the sweep returns our task");
    assert_eq!(ours.status, "dead");
    assert!(
        ours.error.as_deref().unwrap_or_default().contains("device"),
        "the dead-letter must say a device died, not invent an action failure: {:?}",
        ours.error
    );
    Ok(())
}
