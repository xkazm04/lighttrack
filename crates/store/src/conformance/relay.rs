//! `Surface::Relay`: the cloud→device task queue (docs/RELAY.md).

use chrono::Utc;
use serde_json::{json, Value};

use lighttrack_core::{new_id, RelayOutcome, RelayTask};

use crate::{Result, Store};

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

/// Relay queue (docs/RELAY.md): enqueue → lease → settle round-trips, retry/deferral accounting,
/// and the dead-letter sweep. Like the job claim, lease/sweep are global (oldest-due first), so on a
/// shared DB we assert on our ids and tolerate other rows in the results.
pub(super) fn relay(store: &dyn Store, pid: &str) -> Result<()> {
    fn leased_ours(store: &dyn Store, id: &str) -> Result<Option<RelayTask>> {
        Ok(store
            .lease_relay_tasks("conf-dev", 60, 20)?
            .into_iter()
            .find(|t| t.id == id))
    }

    let mut t = sample_task(pid, 2);
    t.idempotency_key = Some(new_id());
    store.create_relay_task(&t)?;

    // Round-trip + idempotency lookup.
    let got = store.get_relay_task(&t.id)?.expect("get_relay_task Some");
    assert_eq!(got.payload, json!({ "k": "v" }), "relay payload round-trip");
    let key = t.idempotency_key.clone().unwrap();
    assert_eq!(
        store.find_relay_task_by_key(pid, &key)?.expect("by key").id,
        t.id
    );
    assert!(store
        .find_relay_task_by_key("other-project", &key)?
        .is_none());

    // Lease consumes an attempt; a failure requeues (zero interval ⇒ due again) with the error.
    let leased = leased_ours(store, &t.id)?.expect("our task leased");
    assert_eq!(leased.status, "leased");
    assert_eq!(leased.attempts, 1);
    let requeued = store
        .settle_relay_task(&t.id, &RelayOutcome::Failed("conf boom".into()))?
        .expect("settle failed");
    assert_eq!(requeued.status, "queued");
    assert_eq!(requeued.error.as_deref(), Some("conf boom"));

    // A deferral hands the consumed attempt back.
    assert_eq!(leased_ours(store, &t.id)?.expect("re-leased").attempts, 2);
    let deferred = store
        .settle_relay_task(
            &t.id,
            &RelayOutcome::Deferred {
                retry_after_secs: Some(0),
                reason: Some("window".into()),
            },
        )?
        .expect("settle deferred");
    assert_eq!(deferred.status, "queued");
    assert_eq!(deferred.attempts, 1, "deferral hands the attempt back");

    // Success is terminal; a duplicate report returns the settled row unchanged.
    leased_ours(store, &t.id)?.expect("leased again");
    let done = store
        .settle_relay_task(&t.id, &RelayOutcome::Succeeded(json!({ "ok": true })))?
        .expect("settle succeeded");
    assert_eq!(done.status, "succeeded");
    assert_eq!(
        done.result,
        json!({ "ok": true }),
        "relay result round-trip"
    );
    let dup = store
        .settle_relay_task(&t.id, &RelayOutcome::Failed("late".into()))?
        .expect("duplicate settle");
    assert_eq!(dup.status, "succeeded", "duplicate report is a no-op");
    assert!(store
        .list_relay_tasks(Some(pid), Some("succeeded"), 100)?
        .iter()
        .any(|x| x.id == t.id));

    // Exhausted failure dead-letters…
    let doomed = sample_task(pid, 1);
    store.create_relay_task(&doomed)?;
    leased_ours(store, &doomed.id)?.expect("doomed leased");
    let dead = store
        .settle_relay_task(&doomed.id, &RelayOutcome::Failed("final".into()))?
        .expect("settle dead");
    assert_eq!(dead.status, "dead");

    // …and so does the sweep, when a vanished device's expired lease has no attempts left.
    let vanished = sample_task(pid, 1);
    store.create_relay_task(&vanished)?;
    let held = store.lease_relay_tasks("conf-dev", 0, 20)?; // zero-second lease: expires at once
    assert!(
        held.iter().any(|x| x.id == vanished.id),
        "vanished task leased"
    );
    let swept = store.sweep_relay_dead()?;
    let ours = swept
        .iter()
        .find(|x| x.id == vanished.id)
        .expect("sweep returns our task");
    assert_eq!(ours.status, "dead");
    assert_eq!(
        ours.error.as_deref(),
        Some("lease expired without a result")
    );
    Ok(())
}
