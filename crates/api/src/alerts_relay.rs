//! One extra alert kind on [`Alerter`]: **`relay_task_unroutable`** (M18).
//!
//! Admission at the enqueue door refuses a task no enrolled device advertises, which catches the
//! typo. It cannot catch the other shape of the same failure: a task that *was* routable when it
//! was accepted and is not any more — the only device that had the action was revoked, went to a
//! narrower capability list on its next upgrade, or was never re-enrolled after a rebuild. Nothing
//! is wrong with that task; there is simply nobody left to run it, and it will sit queued until it
//! ages out of somebody's patience rather than out of a budget.
//!
//! It lives in its own file rather than in `alerts.rs` because that module is being restructured
//! concurrently (M3), and because the delivery here reads its own channel config: `AlertConfig` and
//! the `channels` transport are private to the `alerts` module, and reaching into them from outside
//! would mean editing it. What is *not* duplicated is the part that carries state — the cooldown
//! gate ([`Alerter::should_send_key`]) and the enabled check — so an unroutable alert dedupes
//! against the same window every other alert does.

use std::sync::Arc;

use serde::Serialize;
use serde_json::json;

use crate::alerts::Alerter;

/// One action type nothing in the fleet can run, and how much work is stuck behind it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UnroutableActions {
    pub(crate) action_type: String,
    /// How many queued tasks name it.
    pub(crate) tasks: u32,
    /// How long the oldest of them has been waiting, in seconds.
    pub(crate) oldest_secs: i64,
    /// How many devices are enrolled at all — the number that tells an operator whether the fix is
    /// "enrol a device" or "widen one's capabilities".
    pub(crate) enrolled_devices: u32,
}

impl Alerter {
    /// Fire best-effort `relay_task_unroutable` alerts, deduped per action type on the shared
    /// cooldown so a permanently-stuck queue reports once a window rather than once a minute.
    pub(crate) fn notify_relay_unroutable(self: &Arc<Self>, stuck: &[UnroutableActions]) {
        if !self.enabled() {
            return;
        }
        let due: Vec<UnroutableActions> = stuck
            .iter()
            .filter(|s| self.should_send_key(&format!("relay-unroutable:{}", s.action_type)))
            .cloned()
            .collect();
        if due.is_empty() {
            return;
        }
        tokio::spawn(async move { deliver(&due).await });
    }
}

/// Where an unroutable alert goes. Read at delivery time from the same two env vars the rest of the
/// alerting uses, so an operator configures one thing and gets every alert kind.
struct Channels {
    webhook: Option<String>,
    ntfy: Option<String>,
}

impl Channels {
    fn from_env() -> Channels {
        Channels {
            webhook: env_opt("LIGHTTRACK_ALERT_WEBHOOK"),
            ntfy: env_opt("LIGHTTRACK_ALERT_NTFY"),
        }
    }
}

fn env_opt(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.trim().is_empty())
}

/// Best-effort delivery: a down sink logs and is dropped, exactly like every other alert path — an
/// alert channel must never be able to fail a sweep.
async fn deliver(stuck: &[UnroutableActions]) {
    let ch = Channels::from_env();
    if ch.webhook.is_none() && ch.ntfy.is_none() {
        return;
    }
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_default();
    for s in stuck {
        let msg = message(s);
        if let Some(url) = &ch.webhook {
            let body = json!({
                "event": "relay_task_unroutable", "text": &msg, "content": &msg,
                "unroutable": s,
            });
            post(&http, url, "webhook", body.to_string(), true).await;
        }
        if let Some(url) = &ch.ntfy {
            post(&http, url, "ntfy", msg, false).await;
        }
    }
}

/// The alert text. It names the fix, because "unroutable" on its own sends an operator to the wrong
/// half of the system: the fleet is what changed, not the app that enqueued the work.
fn message(s: &UnroutableActions) -> String {
    let mins = (s.oldest_secs / 60).max(1);
    let fix = if s.enrolled_devices == 0 {
        "no devices are enrolled at all — enrol one (POST /v1/relay/devices)".to_string()
    } else {
        format!(
            "{} device(s) are enrolled and none advertises it — add it to a device's capabilities, \
             or check that the device that used to run it has not been revoked",
            s.enrolled_devices
        )
    };
    format!(
        "LightTrack alert: {} queued relay task(s) for action '{}' have no eligible device \
         (oldest waiting {}m). {}.",
        s.tasks, s.action_type, mins, fix
    )
}

async fn post(http: &reqwest::Client, url: &str, channel: &str, body: String, json: bool) {
    let req = if json {
        http.post(url)
            .header("content-type", "application/json")
            .body(body)
    } else {
        http.post(url).body(body)
    };
    match req.send().await {
        Ok(r) if !r.status().is_success() => {
            tracing::warn!(channel, event = "relay_task_unroutable", status = %r.status(), "alert delivery rejected")
        }
        Err(e) => {
            tracing::warn!(channel, event = "relay_task_unroutable", error = %e, "alert delivery failed")
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stuck(enrolled: u32) -> UnroutableActions {
        UnroutableActions {
            action_type: "xprice/reprice-summary".into(),
            tasks: 3,
            oldest_secs: 3_600,
            enrolled_devices: enrolled,
        }
    }

    #[test]
    fn the_message_names_the_actual_fix_which_depends_on_whether_a_fleet_exists() {
        // Nobody enrolled: the operator needs to enrol, not to edit a capability list.
        let m = message(&stuck(0));
        assert!(m.contains("no devices are enrolled"), "{m}");
        assert!(m.contains("/v1/relay/devices"), "{m}");

        // A fleet that exists and cannot run it: the fix is on a device, and revocation is the
        // likeliest cause of a task that used to route and now does not.
        let m = message(&stuck(2));
        assert!(m.contains("2 device(s) are enrolled"), "{m}");
        assert!(m.contains("revoked"), "{m}");
        // Either way the alert names the action and how much is stuck behind it.
        assert!(
            m.contains("xprice/reprice-summary") && m.contains("3 queued"),
            "{m}"
        );
        assert!(m.contains("60m"), "the wait is reported in minutes: {m}");
    }
}
