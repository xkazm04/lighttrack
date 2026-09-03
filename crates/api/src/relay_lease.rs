//! The relay's lease doors: lease, renew, progress, cancel.
//!
//! `lease_secs` used to be two quantities wearing one name — how long a run may legitimately take,
//! and how long a vanished device may go unnoticed — which is why it was clamped to six hours and
//! why nothing dead-lettered until somebody polled. Now the holder renews on a timer, so the lease
//! is detection latency alone and the response tells the device how often to prove it is alive.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::{LeaseHeld, RelayCancel, RelayTask};

use crate::auth::Principal;
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::relay_devices::{ensure_device, DeviceIdentity};
use crate::state::{spawn_db, AppState};

/// Bounds on a requested lease. The ceiling is no longer "the longest a run may take" — a renewing
/// device holds a task for as long as it needs — it is the longest a *dead* device's task may sit
/// unreclaimed, so it is now minutes rather than the old six hours.
const MIN_LEASE_SECS: i64 = 60;
const MAX_LEASE_SECS: i64 = 1_800;

/// How often the holder should renew, given the lease TTL: a third, the same arithmetic the job
/// worker uses. At TTL/3 a device can miss two consecutive renewals — a sleeping laptop's wake-up, a
/// transient network error — and still hold its task; a cadence at the TTL turns every hiccup into a
/// spurious takeover.
fn renew_secs(lease_secs: i64) -> u64 {
    (lease_secs.max(3) as u64 / 3).max(1)
}

#[derive(Deserialize)]
pub(crate) struct LeaseReq {
    /// What this device can actually run: exact action types, or `"<ns>/*"` (M18). The lease is
    /// narrowed to them, so an action never reaches a device whose library lacks it.
    ///
    /// **Empty means no filter**, which is what a pre-M18 agent and the legacy shared key send: a
    /// device that suddenly leased nothing after an upgrade would be a worse failure than an
    /// unfiltered one.
    #[serde(default)]
    capabilities: Vec<String>,
    /// The `lt-agent` version, recorded on the device so an operator can tell an un-upgraded fleet
    /// from an upgraded one.
    #[serde(default)]
    agent_version: Option<String>,
    #[serde(default = "default_max")]
    max: usize,
    #[serde(default = "default_lease_secs")]
    lease_secs: i64,
    /// Long-poll: hold the request up to this many seconds until a task is due (0 = return
    /// immediately). Cuts pickup latency without shrinking the device's poll interval.
    #[serde(default)]
    wait_secs: u64,
}

fn default_max() -> usize {
    1
}

fn default_lease_secs() -> i64 {
    600
}

/// What a lease hands back: the tasks, and the renewal contract that comes with them.
#[derive(Serialize)]
pub(crate) struct LeaseResp {
    tasks: Vec<RelayTask>,
    /// Seconds between renewals the device is expected to keep to. Sent rather than assumed: the
    /// server owns the TTL (it clamps what was asked for), so a device that guessed its own cadence
    /// would be guessing against a number it cannot see.
    renew_secs: u64,
    /// The TTL actually granted, after clamping.
    lease_secs: i64,
}

pub(crate) async fn lease_tasks(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LeaseReq>,
) -> Result<Json<LeaseResp>, ApiError> {
    let identity = ensure_device(&st, &headers).await?;
    // Identity comes from the KEY, never from the body (M18). The `device` field callers used to
    // send was a client assertion the cloud wrote down as fact, so the record of which machine ran
    // what was decoration; it is ignored now rather than rejected, so an older agent still leases.
    let device = identity.task_device();
    let capabilities = req.capabilities.clone();
    // The heartbeat happens once per lease request, before the long poll: liveness must not wait on
    // there being work, or a healthy idle device would read as a dead one — the same rule that keeps
    // progress off the renewal endpoint.
    touch(&st, &identity, &capabilities, req.agent_version.as_deref()).await;
    let lease_secs = req.lease_secs.clamp(MIN_LEASE_SECS, MAX_LEASE_SECS);
    let max = req.max.clamp(1, 20);
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(req.wait_secs.min(25));
    loop {
        // Sweep first so exhausted expired leases dead-letter (and alert) instead of lingering.
        // The scheduled sweep (`schedule_sweep`) now does this on a timer too — this call stays
        // because it costs one statement and makes the lease path self-healing even with the timed
        // sweep disabled.
        let store = st.store.clone();
        let dead = spawn_db(move || store.sweep_relay_dead()).await?;
        if !dead.is_empty() {
            st.alerts.notify_relay_dead(&dead);
        }
        let store = st.store.clone();
        let device = device.clone();
        let caps = capabilities.clone();
        let tasks =
            spawn_db(move || store.lease_relay_tasks(&device, &caps, lease_secs, max)).await?;
        if !tasks.is_empty() || std::time::Instant::now() >= deadline {
            return Ok(Json(LeaseResp {
                tasks,
                renew_secs: renew_secs(lease_secs),
                lease_secs,
            }));
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// Record that an enrolled device is alive, and what it now advertises. Best-effort: a fleet the
/// backend cannot store must not stop the relay working, and this is bookkeeping, not the lease.
async fn touch(
    st: &AppState,
    identity: &DeviceIdentity,
    capabilities: &[String],
    agent_version: Option<&str>,
) {
    let DeviceIdentity::Enrolled(device) = identity else {
        return; // the legacy shared key has no row to touch
    };
    let store = st.store.clone();
    let (id, caps, ver) = (
        device.id.clone(),
        capabilities.to_vec(),
        agent_version.map(str::to_string),
    );
    if let Err(e) = spawn_db(move || store.touch_device(&id, &caps, ver.as_deref())).await {
        tracing::debug!(device = %device.id, error = %e, "relay lease: device heartbeat not recorded");
    }
}

#[derive(Deserialize)]
pub(crate) struct FenceReq {
    /// The `lease_fence` the device was handed at lease time — its proof the task is still its own.
    fence: DateTime<Utc>,
}

/// Heartbeat: "I am still running this, extend my lease." A **409** means the lease is no longer
/// theirs (it expired and was reclaimed, or an operator cancelled the task) and the device must
/// stop rather than keep running and deliver a result through a connector on nobody's authority.
///
/// The endpoint carries nothing but liveness on purpose — progress rides `/progress`, so a stall in
/// computing something to report can never stall the heartbeat and make a live device read as dead.
pub(crate) async fn renew_lease(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<FenceReq>,
) -> Result<Json<LeaseHeld>, ApiError> {
    ensure_device(&st, &headers).await?;
    let store = st.store.clone();
    let id2 = id.clone();
    let lease = MAX_LEASE_SECS.min(default_lease_secs());
    let held = spawn_db(move || store.renew_relay_lease(&id2, req.fence, lease)).await?;
    verdict(held, &id, "renew")
}

#[derive(Deserialize)]
pub(crate) struct ProgressReq {
    fence: DateTime<Utc>,
    progress: String,
}

pub(crate) async fn post_progress(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<ProgressReq>,
) -> Result<Json<LeaseHeld>, ApiError> {
    ensure_device(&st, &headers).await?;
    let store = st.store.clone();
    let id2 = id.clone();
    let held =
        spawn_db(move || store.update_relay_progress(&id2, req.fence, &req.progress)).await?;
    verdict(held, &id, "progress report")
}

/// Stop a queued or running device task. A queued task is cancelled outright; a leased one is
/// marked `cancelling` and its device learns at the next renewal. Cancelling a task that already
/// finished is a **409**, not a silent success: the operator needs to know nothing was stopped.
///
/// Reachable by the task's own project key or an admin — an operator cancelling their own work, not
/// the device reporting on it, so this is the one lease door that is not device-authenticated.
pub(crate) async fn cancel_task(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<RelayCancel>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    if !matches!(p, Principal::Project { .. }) {
        ensure_can_admin(&p)?;
    }
    let store = st.store.clone();
    let id2 = id.clone();
    // The scope IS the authorization (M17): a task outside it is not found, not refused. The read
    // stays because the handler still needs the row's status for its response.
    let sc = p.scope_owned();
    let _task = spawn_db(move || store.get_relay_task(sc.as_deref().into(), &id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("relay task '{id}' not found")))?;
    let store = st.store.clone();
    let id2 = id.clone();
    let sc = p.scope_owned();
    let outcome = spawn_db(move || store.cancel_relay_task(sc.as_deref().into(), &id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("relay task '{id}' not found")))?;
    if let RelayCancel::AlreadyFinished { status } = &outcome {
        return Err(ApiError::conflict(format!(
            "relay task '{id}' is already {status}; nothing was cancelled"
        )));
    }
    Ok(Json(outcome))
}

/// Turn a lease verdict into an HTTP answer: held is 200, lost is 409 (stop working), gone is 404.
fn verdict(held: LeaseHeld, id: &str, what: &str) -> Result<Json<LeaseHeld>, ApiError> {
    match &held {
        LeaseHeld::Held { .. } => Ok(Json(held)),
        LeaseHeld::NoSuchRecord => Err(ApiError::not_found(format!("relay task '{id}' not found"))),
        LeaseHeld::NotHeld { status, .. } => Err(ApiError::conflict(format!(
            "relay task '{id}' is {status} and no longer held by that lease; stop working on it \
             and do not deliver its result (the {what} was NOT recorded)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::renew_secs;

    #[test]
    fn the_heartbeat_leaves_room_to_miss_a_couple() {
        // A third of the TTL, so two consecutive misses still hold the lease — the same arithmetic
        // the job worker uses, for the same reason.
        assert_eq!(renew_secs(600), 200);
        assert_eq!(renew_secs(60), 20);
        // A nonsensically small TTL still yields a positive cadence rather than a busy loop.
        assert!(renew_secs(1) >= 1);
        assert!(renew_secs(0) >= 1);
    }
}
