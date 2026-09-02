//! Opt-in auto-contribution: ensure a `Contribute` **schedule** exists when the operator asked for
//! one.
//!
//! Recurrence lives in a stored [`Schedule`] (M7), not in a background loop of its own, so the
//! auto-push inherits everything the queue already provides — a lease, a retry budget, cancellation,
//! a job row an operator can read, and `GET /v1/schedules` answering "what runs on a schedule here".
//! This module's whole job is to make sure the row exists, exactly once.
//!
//! **Opt-in, and only opt-in.** Contribution is an act of consent: nothing here runs unless
//! `LIGHTTRACK_COLLECTIVE_AUTO_CONTRIBUTE_SECS` **and** a hub URL are both configured. And the
//! per-project `collective_opt_in` gate still applies underneath — the digest a scheduled push
//! sends is the same one `GET /digest` builds, from consenting projects only.
//!
//! **Idempotent**, because it runs on every boot: a `Contribute` schedule already naming this hub is
//! left alone, including its interval. A boot must not silently reset a cadence an operator changed
//! through `PUT /v1/schedules/:id`.

use chrono::{Duration, Utc};

use lighttrack_core::{
    hub_url_hash, new_id, normalize_hub_url, ContributePayload, JobKind, Schedule,
    SCHEDULE_MIN_INTERVAL_SECS,
};

use crate::state::{spawn_db, AppState};

const ENV_SECS: &str = "LIGHTTRACK_COLLECTIVE_AUTO_CONTRIBUTE_SECS";
const ENV_HUB: &str = "LIGHTTRACK_COLLECTIVE_HUB";
const ENV_KEY_REF: &str = "LIGHTTRACK_COLLECTIVE_HUB_KEY_REF";

/// What the operator asked for, or `None` when auto-contribution is off.
pub(crate) struct AutoContribute {
    hub: String,
    interval_secs: u32,
    hub_key_ref: Option<String>,
}

impl AutoContribute {
    pub(crate) fn from_env() -> Option<Self> {
        let secs = std::env::var(ENV_SECS)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|s| *s > 0)?;
        let hub = std::env::var(ENV_HUB).ok().map(|h| h.trim().to_string())?;
        let hub = normalize_hub_url(&hub).to_string();
        if !(hub.starts_with("http://") || hub.starts_with("https://")) {
            tracing::warn!(
                "{ENV_SECS} is set but {ENV_HUB} is not an absolute http(s) URL; auto-contribution \
                 is OFF"
            );
            return None;
        }
        Some(Self {
            hub,
            interval_secs: secs.max(SCHEDULE_MIN_INTERVAL_SECS),
            hub_key_ref: std::env::var(ENV_KEY_REF)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
        })
    }

    pub(crate) fn describe(cfg: &Option<Self>) -> String {
        match cfg {
            None => format!("off ({ENV_SECS} unset)"),
            // The hub's HASH, not its URL: a boot banner is copied into issues and chat.
            Some(c) => format!("every {}s to {}", c.interval_secs, hub_url_hash(&c.hub)),
        }
    }
}

/// Create the `Contribute` schedule if this deployment has none for that hub. Returns whether one
/// was created. Never propagates: a backend without the `Schedules` surface answers `Unsupported`,
/// which is a declared capability gap and not a reason to refuse to boot.
pub(crate) async fn ensure_schedule(st: &AppState, cfg: &AutoContribute) -> bool {
    let payload = match serde_json::to_value(ContributePayload {
        hub: cfg.hub.clone(),
        hub_key_ref: cfg.hub_key_ref.clone(),
        min_cases: None,
    }) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "could not build the auto-contribute payload");
            return false;
        }
    };
    let hub = cfg.hub.clone();
    let interval = cfg.interval_secs;
    let store = st.store.clone();
    let created = spawn_db(move || {
        // The ledger has no project scope, but a `Schedule` does, so the row is filed under the
        // first project that exists. It carries no project semantics — the digest it pushes spans
        // every consenting project — and a deployment with no projects has nothing to contribute
        // anyway.
        let projects = store.list_projects()?;
        let Some(owner) = projects.first() else {
            return Ok(false);
        };
        for p in &projects {
            for s in store.list_schedules(&p.id)? {
                let same_hub = s.payload.get("hub").and_then(|v| v.as_str()) == Some(hub.as_str());
                if s.kind == JobKind::Contribute.as_str() && same_hub {
                    return Ok(false);
                }
            }
        }
        let now = Utc::now();
        store.create_schedule(&Schedule {
            id: new_id(),
            project_id: owner.id.clone(),
            kind: JobKind::Contribute.as_str().to_string(),
            payload,
            interval_secs: interval,
            // Due one interval out, not now: a redeploy must not push the moment it boots. The
            // hash gate would make that harmless, but "harmless" is not the same as "expected".
            next_due: now + Duration::seconds(interval as i64),
            last_job_id: None,
            enabled: true,
            created_at: now,
        })?;
        Ok(true)
    })
    .await;
    match created {
        Ok(true) => {
            tracing::info!(
                hub = %hub_url_hash(&cfg.hub),
                interval_secs = cfg.interval_secs,
                "created the opt-in auto-contribute schedule; pushes are hash-gated, so an \
                 unchanged digest costs no HTTP call"
            );
            true
        }
        Ok(false) => false,
        Err(e) => {
            tracing::debug!(error = %e, "auto-contribute schedule not created");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boot line must not print the hub's address: banners get pasted into issues and chat.
    #[test]
    fn the_boot_line_names_the_hub_by_hash_not_by_url() {
        let cfg = Some(AutoContribute {
            hub: "https://hub.example".into(),
            interval_secs: 3600,
            hub_key_ref: None,
        });
        let d = AutoContribute::describe(&cfg);
        assert!(d.contains("3600"), "{d}");
        assert!(!d.contains("hub.example"), "{d}");
        assert!(d.contains(&hub_url_hash("https://hub.example")), "{d}");
        assert!(AutoContribute::describe(&None).contains("off"));
    }
}
