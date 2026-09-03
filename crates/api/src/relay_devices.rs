//! The relay device fleet: enrolment, the fleet listing, revocation — and the guard every
//! device-authenticated door goes through (M18, docs/RELAY.md).
//!
//! Enrolment was one shared `LIGHTTRACK_RELAY_DEVICE_KEY`. That is a workable answer for exactly
//! one device and a bad one for two: the secret cannot be rotated for a single machine, a leak
//! means re-keying the whole fleet at once, and the `device` written onto a task was whatever the
//! client asserted — so the cloud's record of *who ran what* was decoration.
//!
//! A device key is `ltd_<prefix>_<secret>`, minted here, shown once, and stored as the same salted
//! digest an API key is ([`crate::auth`]). The legacy shared key still authenticates, as a
//! deprecated single-device fallback with every capability, and says so at startup — a relay that
//! stopped leasing the moment this shipped would be this feature breaking the thing it hardens.
//!
//! **Never over MCP.** This module mints a secret; `docs/RELAY.md` and CLAUDE.md both say
//! secret-minting stays off the agent surface, because a key in a tool result is a key in a
//! transcript.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use lighttrack_core::{new_id, Device};

use crate::auth::{device_prefix_of, generate_device_key, secret_eq, verify_key};
use crate::error::ApiError;
use crate::guards::{authenticate, bearer, ensure_can_admin};
use crate::state::{spawn_db, AppState};

/// How long after its last call-in a device still reads as online. There is no inbound path to a
/// device, so liveness can only ever be "when did it last talk to us" — and a leasing agent talks
/// at least once per poll interval (15s by default) even when it has nothing to do.
const ONLINE_WITHIN_SECS: i64 = 300;

/// Who is driving a device-authenticated door.
///
/// The distinction is load-bearing on the lease: an enrolled device leases under its own id and its
/// own advertised capabilities, while the legacy fallback has neither — so it leases unfiltered,
/// exactly as it did before this shipped.
pub(crate) enum DeviceIdentity {
    /// A key from the `devices` table resolved to this device.
    Enrolled(Box<Device>),
    /// The deprecated shared `LIGHTTRACK_RELAY_DEVICE_KEY`, or an admin/dev principal (which is how
    /// the relay is driven from a local test). No identity and no advertised capabilities.
    Legacy,
}

impl DeviceIdentity {
    /// The id to stamp on a leased task. The legacy fallback keeps writing the name it always did,
    /// so an upgrade does not rewrite the meaning of rows already in the table.
    pub(crate) fn task_device(&self) -> String {
        match self {
            DeviceIdentity::Enrolled(d) => d.id.clone(),
            DeviceIdentity::Legacy => LEGACY_DEVICE_NAME.to_string(),
        }
    }
}

/// What `relay_tasks.device` carried before enrolment existed, kept verbatim for the fallback.
pub(crate) const LEGACY_DEVICE_NAME: &str = "default";

/// Authenticate a device door: an enrolled `ltd_…` key, the deprecated shared key, or an admin.
///
/// The order matters. A token that *is* a device key (by scheme) is resolved against the table and
/// **refused outright** if it does not check out — falling through to the admin path there would
/// turn a revoked device's key into a request that gets a different, confusing error. A token that
/// is not a device key at all falls through, which is what keeps the legacy key and local admin
/// testing working.
pub(crate) async fn ensure_device(
    st: &AppState,
    headers: &HeaderMap,
) -> Result<DeviceIdentity, ApiError> {
    if let Some(token) = bearer(headers) {
        if let Some(prefix) = device_prefix_of(&token) {
            return resolve_enrolled(st, &prefix, &token).await;
        }
        if let Some(expected) = st.relay_device_key.as_ref() {
            // Constant-time for the same reason the admin key is: an operator-chosen secret
            // compared against raw presented bytes, so a short-circuiting `==` is a byte-at-a-time
            // oracle. A *wrong* key falls through to `authenticate`, which meters the failure.
            if secret_eq(&token, expected) {
                return Ok(DeviceIdentity::Legacy);
            }
        }
    }
    ensure_can_admin(&authenticate(st, headers).await?)?;
    Ok(DeviceIdentity::Legacy)
}

async fn resolve_enrolled(
    st: &AppState,
    prefix: &str,
    token: &str,
) -> Result<DeviceIdentity, ApiError> {
    let store = st.store.clone();
    let prefix = prefix.to_string();
    let found = match spawn_db(move || store.find_device_by_key_prefix(&prefix)).await {
        Ok(v) => v,
        // A backend that does not serve the fleet cannot resolve a device key. That is a
        // deployment shape, not a credential problem, so it is said plainly rather than as a 401
        // the operator would spend an afternoon on.
        Err(e) => return Err(e),
    };
    let Some(device) = found else {
        return Err(ApiError::unauthorized("unknown device key"));
    };
    if !verify_key(&device.key_hash, token) {
        return Err(ApiError::unauthorized("invalid device key"));
    }
    if device.revoked {
        return Err(ApiError::unauthorized(format!(
            "device '{}' has been revoked",
            device.name
        )));
    }
    Ok(DeviceIdentity::Enrolled(Box::new(device)))
}

/// Say, once at startup, that the shared device key is a deprecated fallback.
///
/// It authenticates with **every capability**, which is precisely what enrolment exists to stop: a
/// fleet on the shared key gets no routing, no revocation, and no liveness. Not a warning block
/// like the auth-mode banner — this is a working configuration, just the old one.
pub(crate) fn warn_if_legacy_key(configured: bool) {
    if !configured {
        return;
    }
    tracing::warn!(
        "LIGHTTRACK_RELAY_DEVICE_KEY is set: this instance still accepts the DEPRECATED shared \
         relay device key, which authenticates with every capability and cannot be revoked, \
         routed, or told apart from any other device holding it. Enrol devices instead \
         (POST /v1/relay/devices) and unset it — it is kept for one release."
    );
}

#[derive(Deserialize)]
pub(crate) struct CreateDeviceReq {
    name: String,
    /// Scope the device to one project, or omit for an operator-wide device that serves every
    /// project's tasks — the shape the shipped single-device relay already had.
    #[serde(default)]
    project_id: Option<String>,
    /// What this device may run: exact action types, or `"<ns>/*"`. Omitted means "everything",
    /// which is the honest reading of an advertisement nobody made — and it is what the device's
    /// own inventory will replace at its first lease.
    #[serde(default)]
    capabilities: Vec<String>,
}

/// A freshly enrolled device. `key` appears here and nowhere else, ever again.
#[derive(Serialize)]
pub(crate) struct CreatedDevice {
    #[serde(flatten)]
    device: Device,
    /// The raw `ltd_…` key. **Shown once**: only its salted digest is stored, so a lost key is
    /// re-enrolled, never recovered.
    key: String,
}

pub(crate) async fn create_device(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateDeviceReq>,
) -> Result<Json<CreatedDevice>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("name is required"));
    }
    let generated = generate_device_key();
    let device = Device {
        id: new_id(),
        project_id: req.project_id.filter(|p| !p.trim().is_empty()),
        name: name.to_string(),
        key_prefix: generated.prefix,
        key_hash: generated.key_hash,
        capabilities: req
            .capabilities
            .into_iter()
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect(),
        last_seen_at: None,
        agent_version: None,
        created_at: Utc::now(),
        revoked: false,
    };
    let store = st.store.clone();
    let d2 = device.clone();
    spawn_db(move || store.create_device(&d2)).await?;
    Ok(Json(CreatedDevice {
        // The stored digest never leaves the database, not even to the operator who just minted it.
        device: device.redacted(),
        key: generated.full_key,
    }))
}

#[derive(Deserialize)]
pub(crate) struct ListParams {
    project: Option<String>,
}

/// One device as an operator reads it: the row, minus the digest, plus the liveness the fleet
/// listing exists to show.
#[derive(Serialize)]
pub(crate) struct DeviceView {
    #[serde(flatten)]
    device: Device,
    /// Seconds since this device last leased, renewed or reported; `None` = it never has.
    seen_secs_ago: Option<i64>,
    /// Whether that was recent enough to call it alive. A revoked device is never online, whatever
    /// its timestamp says — it cannot lease again.
    online: bool,
}

pub(crate) async fn list_devices(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListParams>,
) -> Result<Json<Vec<DeviceView>>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let project = q.project;
    let devices = spawn_db(move || store.list_devices(project.as_deref().into())).await?;
    let now = Utc::now();
    Ok(Json(devices.iter().map(|d| view(d, now)).collect()))
}

fn view(d: &Device, now: chrono::DateTime<Utc>) -> DeviceView {
    let seen_secs_ago = d.last_seen_at.map(|t| (now - t).num_seconds());
    DeviceView {
        online: !d.revoked && seen_secs_ago.is_some_and(|s| s <= ONLINE_WITHIN_SECS),
        seen_secs_ago,
        device: d.redacted(),
    }
}

pub(crate) async fn revoke_device(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Device>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let store = st.store.clone();
    let id2 = id.clone();
    let sc = p.scope_owned();
    if !spawn_db(move || store.revoke_device(sc.as_deref().into(), &id2)).await? {
        return Err(ApiError::not_found(format!("device '{id}' not found")));
    }
    // Read back rather than reporting success blind: revocation is a security action, and the
    // operator is entitled to see the row that now says `revoked`.
    let store = st.store.clone();
    let id2 = id.clone();
    let sc = p.scope_owned();
    let device = spawn_db(move || store.get_device(sc.as_deref().into(), &id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("device '{id}' not found")))?;
    Ok(Json(device.redacted()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(last_seen: Option<chrono::DateTime<Utc>>, revoked: bool) -> Device {
        Device {
            id: "d1".into(),
            project_id: None,
            name: "laptop".into(),
            key_prefix: "abcd1234".into(),
            key_hash: "salt:digest".into(),
            capabilities: vec!["xprice/*".into()],
            last_seen_at: last_seen,
            agent_version: None,
            created_at: Utc::now(),
            revoked,
        }
    }

    #[test]
    fn liveness_is_last_contact_and_a_revoked_device_is_never_online() {
        let now = Utc::now();
        let fresh = view(
            &sample(Some(now - chrono::Duration::seconds(30)), false),
            now,
        );
        assert!(fresh.online);
        assert_eq!(fresh.seen_secs_ago, Some(30));

        let stale = view(&sample(Some(now - chrono::Duration::hours(2)), false), now);
        assert!(!stale.online, "two hours of silence is not a live device");

        // Never called in: no timestamp to report, and certainly not online.
        let never = view(&sample(None, false), now);
        assert!(!never.online);
        assert!(never.seen_secs_ago.is_none());

        // Revoked is offline whatever the clock says — it cannot lease again, so reporting it as
        // alive would be a comfortable lie in the one surface an operator checks after revoking.
        let revoked = view(&sample(Some(now), true), now);
        assert!(!revoked.online);
    }

    #[test]
    fn the_stored_digest_never_reaches_a_response() {
        let v = view(&sample(None, false), Utc::now());
        assert!(v.device.key_hash.is_empty());
        let json = serde_json::to_value(&v).expect("serialize device view");
        assert_eq!(json["key_hash"], "");
        assert!(
            json.get("key").is_none(),
            "only enrolment ever carries a key"
        );
    }
}
