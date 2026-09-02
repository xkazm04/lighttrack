//! The half of `Surface::Relay` that M7 added: the fence, the renewal, progress, and cancellation.
//!
//! The headline case is `late_settle_after_reclaim_is_not_held`. Before the fence, `settle` asked
//! only "is this task leased?" — which a task re-leased to a *second* device answers yes to, so a
//! slow device's late report landed on the run in progress and overwrote it. There is no way to
//! observe that from outside; it has to be proven here.

use lighttrack_core::{new_id, LeaseHeld, RelayCancel, RelayOutcome, RelaySettle};
use serde_json::json;

use super::relay::{leased_ours, sample_task, settled};
use crate::{Result, Store};

/// Renewal, progress, and the late settle a fence exists to refuse.
pub(super) fn relay_fencing(store: &dyn Store, pid: &str) -> Result<()> {
    let t = sample_task(pid, 3);
    store.create_relay_task(&t)?;
    let first = leased_ours(store, &t.id, 0)?.expect("first device leases");
    let stale_fence = first.lease_fence.expect("a lease stamps a fence");

    // The device is alive: renewing extends the DEADLINE and keeps the fence, so the token it
    // reports with hours later is the one it was handed.
    let renewed = store.renew_relay_lease(&t.id, stale_fence, 3600)?;
    let deadline = match renewed {
        LeaseHeld::Held { deadline } => deadline.expect("a renewal returns the new deadline"),
        other => panic!("a live holder's renewal must land, got {other:?}"),
    };
    let after = store.get_relay_task(&t.id)?.expect("get after renew");
    assert_eq!(
        after.lease_fence,
        Some(stale_fence),
        "renewal moves the deadline, never the fence"
    );
    assert!(
        after.lease_deadline.expect("deadline") >= deadline - chrono::Duration::seconds(1),
        "renewal must actually extend the deadline"
    );

    // Progress rides its own door and is visible on the task.
    assert!(store
        .update_relay_progress(&t.id, stale_fence, "step 2 of 5")?
        .is_held());
    assert_eq!(
        store
            .get_relay_task(&t.id)?
            .expect("get after progress")
            .progress
            .as_deref(),
        Some("step 2 of 5")
    );

    // Now the device dies: expire the lease and let a second device reclaim the task.
    store.renew_relay_lease(&t.id, stale_fence, 0)?; // deadline in the past
    let second = leased_ours(store, &t.id, 3600)?.expect("a second device reclaims it");
    let live_fence = second.lease_fence.expect("the reclaim mints a NEW fence");
    assert_ne!(
        live_fence, stale_fence,
        "a re-lease is a different lease and must carry a different identity"
    );
    assert!(
        second.progress.is_none(),
        "a new holder must not inherit the dead one's progress"
    );

    // THE case. The first device finally finishes and reports. It is refused — and the second
    // device's task is untouched.
    match store.settle_relay_task(
        &t.id,
        Some(stale_fence),
        &RelayOutcome::Succeeded(json!({ "from": "the zombie" })),
    )? {
        RelaySettle::NotHeld { status, fence } => {
            assert_eq!(status, "leased");
            assert_eq!(fence, Some(live_fence), "…and says who holds it now");
        }
        other => panic!("a reclaimed device's late settle must be refused, got {other:?}"),
    }
    let untouched = store
        .get_relay_task(&t.id)?
        .expect("get after the late settle");
    assert_eq!(untouched.status, "leased");
    assert_eq!(untouched.result, serde_json::Value::Null);

    // The same refusal reaches renewal and progress, so the zombie learns it lost and stops.
    assert!(matches!(
        store.renew_relay_lease(&t.id, stale_fence, 60)?,
        LeaseHeld::NotHeld { .. }
    ));
    assert!(matches!(
        store.update_relay_progress(&t.id, stale_fence, "still going")?,
        LeaseHeld::NotHeld { .. }
    ));
    // An unknown id is a distinct answer from a live task someone else owns.
    assert!(matches!(
        store.renew_relay_lease(&new_id(), stale_fence, 60)?,
        LeaseHeld::NoSuchRecord
    ));

    // The rightful holder still settles.
    let done = settled(
        store.settle_relay_task(&t.id, Some(live_fence), &RelayOutcome::Succeeded(json!(1)))?,
        "the holder settles",
    );
    assert_eq!(done.status, "succeeded");
    Ok(())
}

/// Cancellation from both live states, and the property that matters: a cancelled task is never
/// handed to a second device.
pub(super) fn relay_cancellation(store: &dyn Store, pid: &str) -> Result<()> {
    let queued = sample_task(pid, 3);
    store.create_relay_task(&queued)?;
    assert_eq!(
        store.cancel_relay_task(&queued.id)?,
        Some(RelayCancel::Cancelled),
        "a queued task is cancelled outright — nothing ran"
    );
    assert_eq!(
        store.get_relay_task(&queued.id)?.expect("get").status,
        "cancelled"
    );
    assert!(
        leased_ours(store, &queued.id, 60)?.is_none(),
        "a cancelled task must never be leased"
    );
    // Re-cancelling something terminal must not claim to have stopped it.
    assert!(matches!(
        store.cancel_relay_task(&queued.id)?,
        Some(RelayCancel::AlreadyFinished { .. })
    ));
    // An unknown id is None (→ 404), not a fabricated success.
    assert_eq!(store.cancel_relay_task(&new_id())?, None);

    // A LEASED task: cancel marks it `cancelling` — still live, so the device can renew and report
    // honestly, but outside the leasable set, so the reclaim path cannot start a second copy.
    let running = sample_task(pid, 3);
    store.create_relay_task(&running)?;
    let held = leased_ours(store, &running.id, 0)?.expect("running leased");
    let fence = held.lease_fence.expect("fence");
    assert_eq!(
        store.cancel_relay_task(&running.id)?,
        Some(RelayCancel::Cancelling)
    );
    assert_eq!(
        store.get_relay_task(&running.id)?.expect("get").status,
        "cancelling"
    );
    assert!(
        leased_ours(store, &running.id, 60)?.is_none(),
        "a cancelled run must never be reclaimed as stale — this is the race the fence-less \
         reclaim path used to lose"
    );
    assert!(
        store.renew_relay_lease(&running.id, fence, 60)?.is_held(),
        "a device asked to stop is still running and must keep its lease until it reports"
    );
    // Whatever it reports, the task ends `cancelled`: an operator stopped it, so its outcome is not
    // a verdict on the action and must not consume the retry budget.
    let ended = settled(
        store.settle_relay_task(
            &running.id,
            Some(fence),
            &RelayOutcome::Failed("stopped mid-way".into()),
        )?,
        "settle a cancelling task",
    );
    assert_eq!(ended.status, "cancelled");
    assert_eq!(ended.failures, 0, "cancelling is not failing");
    Ok(())
}
