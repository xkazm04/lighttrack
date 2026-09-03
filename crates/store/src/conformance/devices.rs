//! `Surface::Devices`: the enrolled relay fleet, and the two properties it exists to guarantee —
//! **a capability filter routes correctly**, and **an unroutable task is never leased**.
//!
//! Those two are pinned here rather than in a backend's own tests because they are the whole point
//! of M18 and they are invisible from outside: a device that leases work it cannot run looks
//! exactly like a healthy device until the task dead-letters five hours later, having burned every
//! attempt on "no such action".

use chrono::Utc;

use lighttrack_core::{new_id, Device};

use super::relay::sample_task;
use crate::Scope;
use crate::{Result, Store};

/// A device advertising `capabilities`, enrolled operator-wide. `key_hash` is a stand-in digest:
/// the store never verifies it (the API does), it only has to round-trip.
pub(super) fn sample_device(name: &str, capabilities: &[&str]) -> Device {
    let id = new_id();
    Device {
        key_prefix: id.replace('-', "")[..8].to_string(),
        id,
        project_id: None,
        name: name.to_string(),
        key_hash: "conf-salt:conf-digest".to_string(),
        capabilities: capabilities.iter().map(|s| (*s).to_string()).collect(),
        last_seen_at: None,
        agent_version: None,
        created_at: Utc::now(),
        revoked: false,
    }
}

pub(super) fn devices(store: &dyn Store) -> Result<()> {
    let ns = format!("conf{}", new_id().replace('-', ""));
    let mut d = sample_device("conformance-laptop", &[&format!("{ns}/*")]);
    store.create_device(&d)?;

    // Round-trip, including the capability list and the prefix the key lookup keys on.
    let got = store
        .get_device(Scope::Operator, &d.id)?
        .expect("get_device Some");
    assert_eq!(got.name, d.name);
    assert_eq!(got.capabilities, d.capabilities, "capability round-trip");
    assert!(!got.revoked);
    assert!(
        got.last_seen_at.is_none(),
        "a device that has never called in has no liveness to report"
    );
    assert_eq!(
        store
            .find_device_by_key_prefix(&d.key_prefix)?
            .expect("by prefix")
            .id,
        d.id
    );
    assert!(store.find_device_by_key_prefix(&new_id())?.is_none());
    assert!(
        store
            .list_devices(Scope::Operator)?
            .iter()
            .any(|x| x.id == d.id),
        "an enrolled device must appear in the fleet listing"
    );

    // A heartbeat records liveness and REPLACES the advertisement — the device's own view is
    // authoritative, because a list an operator typed at enrolment goes stale the moment somebody
    // adds an action folder, and a stale list is the routing failure this surface exists to end.
    let widened = vec![format!("{ns}/*"), format!("{ns}-extra/one")];
    store.touch_device(&d.id, &widened, Some("0.9.9"))?;
    let got = store
        .get_device(Scope::Operator, &d.id)?
        .expect("get after touch");
    assert!(
        got.last_seen_at.is_some(),
        "touch_device is the only liveness signal a device behind NAT can give"
    );
    assert_eq!(got.capabilities, widened);
    assert_eq!(got.agent_version.as_deref(), Some("0.9.9"));

    // …but an EMPTY report never blanks the row. A pre-M18 agent advertises nothing, and letting
    // that widen the device to "everything" would silently undo the operator's narrowing.
    store.touch_device(&d.id, &[], None)?;
    let got = store
        .get_device(Scope::Operator, &d.id)?
        .expect("get after empty touch");
    assert_eq!(
        got.capabilities, widened,
        "an empty report is not an erasure"
    );
    assert_eq!(got.agent_version.as_deref(), Some("0.9.9"));

    d.capabilities = widened;
    eligibility(store, &d, &ns)?;
    capability_routed_lease(store, &d, &ns)?;
    revocation(store, &d, &ns)?;
    Ok(())
}

/// Both figures, because the enqueue door treats them oppositely: `enrolled == 0` admits (the
/// legacy shared-key deployment), `enrolled > 0 && eligible == 0` refuses.
fn eligibility(store: &dyn Store, d: &Device, ns: &str) -> Result<()> {
    let e = store.count_eligible_devices(&format!("{ns}/echo"))?;
    assert!(e.enrolled >= 1, "our device is enrolled");
    assert!(e.eligible >= 1, "…and it advertises this namespace");
    assert!(
        !e.admit(&format!("{ns}/echo")).is_refused(),
        "an action a device advertises must be admitted"
    );

    // A namespace nothing advertises: eligible is zero while enrolled is not, which is the
    // refusal. Scoped to a fresh id so a shared database's other devices cannot serve it.
    let orphan = format!("{}/nobody-has-this", new_id().replace('-', ""));
    let e = store.count_eligible_devices(&orphan)?;
    assert_eq!(e.eligible, 0, "nothing advertises {orphan}");
    assert!(
        e.admit(&orphan).is_refused(),
        "a fleet that exists and cannot run this must refuse at the door, not dead-letter later"
    );
    let _ = d;
    Ok(())
}

/// The routing property: a lease carrying a device's capabilities takes only what it can run, and
/// **leaves the rest queued and untouched** — not merely unreturned. A filter applied after the
/// claim would still have stamped a fence and burned an attempt on work the device cannot do.
fn capability_routed_lease(store: &dyn Store, d: &Device, ns: &str) -> Result<()> {
    let pid = new_id();
    let mut mine = sample_task(&pid, 3);
    mine.action_type = format!("{ns}/echo");
    store.create_relay_task(&mine)?;
    let mut theirs = sample_task(&pid, 3);
    theirs.action_type = format!("{ns}-other/echo");
    store.create_relay_task(&theirs)?;

    let leased = store.lease_relay_tasks(&d.id, &d.capabilities, 60, 50)?;
    assert!(
        leased.iter().any(|t| t.id == mine.id),
        "a task inside an advertised namespace must be leasable"
    );
    assert!(
        !leased.iter().any(|t| t.id == theirs.id),
        "a task OUTSIDE the advertised set must never be handed to this device"
    );

    let untouched = store
        .get_relay_task(Scope::Operator, &theirs.id)?
        .expect("get unroutable");
    assert_eq!(untouched.status, "queued", "…and must stay queued");
    assert_eq!(
        untouched.attempts, 0,
        "…having burned no attempt: the filter narrows what is CLAIMABLE, not what is returned"
    );
    assert!(untouched.lease_fence.is_none(), "…and stamped no fence");

    // `xprice/*` must not match `xpriceyy/*`: the prefix stops at a `/`, and a device that
    // advertises a namespace has not advertised every longer name that starts the same way.
    let mut lookalike = sample_task(&pid, 3);
    lookalike.action_type = format!("{ns}extra/echo");
    store.create_relay_task(&lookalike)?;
    let leased = store.lease_relay_tasks(&d.id, &d.capabilities, 60, 50)?;
    assert!(
        !leased.iter().any(|t| t.id == lookalike.id),
        "a namespace prefix must stop at a '/' — '{ns}/*' does not cover '{ns}extra/echo'"
    );

    // An EMPTY advertisement is the back-compat "everything": a pre-M18 agent and the legacy shared
    // device key send none, and a device that suddenly leased nothing after an upgrade would be a
    // worse failure than an unfiltered one.
    let leased = store.lease_relay_tasks("conf-legacy-device", &[], 60, 50)?;
    assert!(
        leased.iter().any(|t| t.id == theirs.id),
        "an unfiltered lease must still take everything due"
    );
    Ok(())
}

/// Revocation is a flag, not a delete: the device still resolves (so a task naming it still reads),
/// and it is eligible for nothing.
fn revocation(store: &dyn Store, d: &Device, ns: &str) -> Result<()> {
    assert!(
        store.revoke_device(Scope::Operator, &d.id)?,
        "revoking a real device"
    );
    assert!(
        !store.revoke_device(Scope::Operator, &new_id())?,
        "revoking a device that does not exist must say so rather than report success"
    );
    let got = store
        .get_device(Scope::Operator, &d.id)?
        .expect("a revoked device still resolves");
    assert!(got.revoked);
    assert!(
        !got.serves(&format!("{ns}/echo")),
        "a revoked device is eligible for nothing, whatever it advertises"
    );
    Ok(())
}
