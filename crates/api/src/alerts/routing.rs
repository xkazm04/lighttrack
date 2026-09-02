//! Where an alert goes: the env-configured **global** channels, unioned with whatever the store
//! holds for the alert's project, then narrowed by each channel's severity floor and kind filter.
//!
//! The env destinations are *synthesised*, never persisted. That is what makes this change
//! invisible to an existing deployment: a server configured with `LIGHTTRACK_ALERT_WEBHOOK` and no
//! stored channels routes exactly as it did, because the synthesised row is a global webhook
//! channel with no severity floor and no kind filter — the old behaviour, spelled as data.
//!
//! This module also owns the admin routes for the stored half:
//! `GET|PUT /v1/projects/:id/alert-channels`, `DELETE /v1/projects/:id/alert-channels/:cid`, and
//! `POST /v1/alert-channels/:id/test`.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use lighttrack_core::{new_id, Alert, AlertChannel, AlertKind, ChannelKind, Severity};

use super::{sign, vet, AlertConfig, Alerter};
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::state::{spawn_db, AppState};
use lighttrack_store::Scope as TenantScope;

/// Synthetic ids for the env-configured channels, so a delivery record can name them.
pub(crate) const ENV_WEBHOOK: &str = "env:webhook";
pub(crate) const ENV_NTFY: &str = "env:ntfy";
pub(crate) const ENV_EMAIL: &str = "env:email";

/// The env destinations as global channel rows. No severity floor and no kind filter: this is the
/// pre-routing behaviour restated, and quietly narrowing it would silence alerts an operator is
/// receiving today.
pub(crate) fn env_channels(cfg: &AlertConfig) -> Vec<AlertChannel> {
    let mut out = Vec::new();
    let mut push = |id: &str, kind: ChannelKind, target: String, key: Option<String>| {
        out.push(AlertChannel {
            id: id.to_string(),
            project_id: None,
            kind,
            target,
            secret_hash: key,
            prev_secret_hash: None,
            min_severity: Severity::Info,
            kinds: Vec::new(),
            enabled: true,
            created_at: Utc::now(),
        });
    };
    if let Some(u) = &cfg.webhook {
        push(
            ENV_WEBHOOK,
            ChannelKind::Webhook,
            u.clone(),
            cfg.webhook_key.clone(),
        );
    }
    if let Some(u) = &cfg.ntfy {
        push(ENV_NTFY, ChannelKind::Ntfy, u.clone(), None);
    }
    if let Some(r) = &cfg.resend {
        // One row for the whole recipient list: Resend takes an array, so this is one delivery.
        push(ENV_EMAIL, ChannelKind::Email, r.to.join(","), None);
    }
    out
}

impl Alerter {
    /// Every channel that should receive `alert`, env ∪ stored, already narrowed.
    ///
    /// A store that does not serve `AlertRouting` answers `Unsupported`; that is a *declared*
    /// limitation, so it degrades to the env channels rather than failing the delivery — an
    /// operator on such a backend still gets the alerts they had before.
    pub(crate) async fn channels_for_alert(&self, alert: &Alert) -> Vec<AlertChannel> {
        let mut all = env_channels(&self.config);
        if let Some(store) = self.store() {
            let project = alert.project_id.clone();
            let stored = spawn_db(move || store.channels_for(project.as_deref().into()))
                .await
                .unwrap_or_default();
            all.extend(stored);
        }
        all.retain(|c| c.accepts(alert.kind, alert.severity));
        all
    }
}

/// The body of `PUT /v1/projects/:id/alert-channels`.
#[derive(Deserialize)]
pub(crate) struct ChannelReq {
    kind: ChannelKind,
    target: String,
    #[serde(default)]
    min_severity: Severity,
    /// Which kinds this channel wants; empty (or absent) = every kind.
    #[serde(default)]
    kinds: Vec<AlertKind>,
    #[serde(default = "yes")]
    enabled: bool,
    /// Sign this channel's webhook deliveries. The server mints the secret and returns it **once**;
    /// only the derived key is stored (see [`sign`]).
    #[serde(default)]
    signed: bool,
}

fn yes() -> bool {
    true
}

pub(crate) async fn put_channel(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<ChannelReq>,
) -> Result<Json<Value>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;

    let store = st.store.clone();
    let check = pid.clone();
    if spawn_db(move || store.get_project(&check)).await?.is_none() {
        return Err(ApiError::not_found(format!("project '{pid}' not found")));
    }

    // A signed channel's secret exists exactly twice: in this response, and in the receiver's
    // config. What is stored is the derived key — see [`sign`].
    let secret = req
        .signed
        .then(|| format!("whsec_{}", new_id().replace('-', "")));
    let channel = AlertChannel {
        id: new_id(),
        project_id: Some(pid),
        kind: req.kind,
        target: req.target,
        secret_hash: secret.as_deref().map(sign::derive_key),
        prev_secret_hash: None,
        min_severity: req.min_severity,
        kinds: req.kinds,
        enabled: req.enabled,
        created_at: Utc::now(),
    };
    channel.validate().map_err(ApiError::bad_request)?;
    // Vetted at configure time so a bad destination is a 400 that says why, rather than a delivery
    // that quietly never lands. It is vetted again before every delivery.
    if matches!(channel.kind, ChannelKind::Webhook | ChannelKind::Ntfy) {
        vet::check(&channel.target, st.alerts.config.dev_destinations)
            .await
            .map_err(ApiError::bad_request)?;
    }

    let store = st.store.clone();
    let c2 = channel.clone();
    spawn_db(move || store.create_alert_channel(&c2)).await?;

    let mut out = serde_json::to_value(channel.redacted())
        .map_err(|e| ApiError::internal(format!("alert channel is not serializable: {e}")))?;
    if let (Some(obj), Some(s)) = (out.as_object_mut(), secret) {
        obj.insert("secret".into(), json!(s));
        obj.insert(
            "secret_note".into(),
            json!(
                "Shown once. Store it in your receiver; LightTrack keeps only sha256(secret) as \
                 the signing key."
            ),
        );
    }
    Ok(Json(out))
}

pub(crate) async fn list_channels(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
) -> Result<Json<Vec<AlertChannel>>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    // The project's own channels *and* the globals it inherits: an operator asking "where do this
    // project's alerts go" is asking about the effective set, not about one table.
    let v = spawn_db(move || store.channels_for(TenantScope::Project(&pid))).await?;
    Ok(Json(v.iter().map(AlertChannel::redacted).collect()))
}

pub(crate) async fn delete_channel(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((_pid, cid)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let store = st.store.clone();
    let id2 = cid.clone();
    let sc = p.scope_owned();
    if !spawn_db(move || store.delete_alert_channel(sc.as_deref().into(), &id2)).await? {
        return Err(ApiError::not_found(format!(
            "alert channel '{cid}' not found"
        )));
    }
    Ok(Json(json!({ "deleted": cid })))
}

/// `POST /v1/alert-channels/:id/test` — send a real, signed test alert down one channel and report
/// what happened. The point is that "did I configure this correctly" is answerable *before* an
/// incident, which is the only time anyone finds out otherwise.
pub(crate) async fn test_channel(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let store = st.store.clone();
    let id2 = id.clone();
    let sc = p.scope_owned();
    let channel = spawn_db(move || store.get_alert_channel(sc.as_deref().into(), &id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("alert channel '{id}' not found")))?;

    let alert = super::compose::bench_run(&super::compose::BenchRunAlert {
        benchmark: "channel-test".into(),
        run_id: id.clone(),
        status: "test".into(),
        mean: None,
        baseline: None,
    });
    let delivery = super::channels::deliver(&st.alerts, &channel, &alert).await;
    Ok(Json(json!({
        "channel_id": channel.id,
        "target": channel.target,
        "signed": channel.secret_hash.is_some(),
        "ok": delivery.ok,
        "status": delivery.status,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg(webhook: Option<&str>, ntfy: Option<&str>) -> AlertConfig {
        AlertConfig {
            webhook: webhook.map(str::to_string),
            bench_webhook: None,
            ntfy: ntfy.map(str::to_string),
            resend: None,
            webhook_key: None,
            cooldown: Duration::from_secs(3600),
            error_threshold: 5,
            error_window: Duration::from_secs(300),
            score_window: 20,
            score_min_samples: 8,
            score_drop: 0.15,
            dev_destinations: false,
        }
    }

    /// The synthesised globals must reproduce the pre-routing behaviour exactly: every kind, every
    /// severity. A floor here would silently stop delivering alerts a deployment gets today.
    #[test]
    fn env_channels_accept_everything_they_used_to() {
        let ch = env_channels(&cfg(
            Some("https://hook.test/x"),
            Some("https://ntfy.test/t"),
        ));
        assert_eq!(ch.len(), 2);
        assert_eq!(ch[0].id, ENV_WEBHOOK);
        assert!(ch[0].project_id.is_none(), "env channels are global");
        for c in &ch {
            for k in AlertKind::ALL {
                assert!(
                    c.accepts(*k, Severity::Info),
                    "{:?} must accept {k:?}",
                    c.id
                );
            }
        }
    }

    #[test]
    fn no_env_destination_synthesises_no_channel() {
        assert!(env_channels(&cfg(None, None)).is_empty());
    }
}
