//! Breach-alert delivery: when an ingested event trips a limit, push the breach to a configured
//! webhook and/or ntfy endpoint.
//!
//! Delivery is **best-effort** and happens **off the request path** (a spawned task), so a slow or
//! down alert sink never delays or fails ingest. Alerts are **deduplicated** per
//! `(project, metric, window)` with a cooldown, so a sustained breach (which trips on every ingest
//! until the rolling window clears) doesn't spam the channel.
//!
//! Config is server-global via env (per-project routing would need schema/Store changes — the
//! breach payload carries `project_id` so a single receiver can route):
//!   LIGHTTRACK_ALERT_WEBHOOK       POST a JSON body (Slack/Discord/custom) on breach
//!   LIGHTTRACK_ALERT_NTFY          POST a text body to an ntfy topic URL on breach
//!   LIGHTTRACK_ALERT_COOLDOWN_SECS re-alert window per (project, metric, window) (default 3600)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lighttrack_core::{LimitStatus, RelayTask};

use crate::forecast::ForecastAlert;

struct AlertConfig {
    webhook: Option<String>,
    ntfy: Option<String>,
    cooldown: Duration,
}

pub(crate) struct Alerter {
    config: AlertConfig,
    http: reqwest::Client,
    last_sent: Mutex<HashMap<String, Instant>>,
}

impl Alerter {
    pub(crate) fn from_env() -> Self {
        let cooldown = std::env::var("LIGHTTRACK_ALERT_COOLDOWN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600);
        Self {
            config: AlertConfig {
                webhook: env_opt("LIGHTTRACK_ALERT_WEBHOOK"),
                ntfy: env_opt("LIGHTTRACK_ALERT_NTFY"),
                cooldown: Duration::from_secs(cooldown),
            },
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            last_sent: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.config.webhook.is_some() || self.config.ntfy.is_some()
    }

    /// One-line summary for the startup banner.
    pub(crate) fn describe(&self) -> String {
        if !self.enabled() {
            return "off".to_string();
        }
        let mut chans = Vec::new();
        if self.config.webhook.is_some() {
            chans.push("webhook");
        }
        if self.config.ntfy.is_some() {
            chans.push("ntfy");
        }
        format!("{} (cooldown {}s)", chans.join("+"), self.config.cooldown.as_secs())
    }

    /// Fire best-effort delivery for the given breaches (after per-key cooldown dedup). Returns
    /// immediately; the actual HTTP happens on a spawned task.
    pub(crate) fn notify(self: &Arc<Self>, breaches: &[LimitStatus]) {
        if !self.enabled() {
            return;
        }
        let due: Vec<LimitStatus> = breaches.iter().filter(|b| self.should_send(b)).cloned().collect();
        if due.is_empty() {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move { me.deliver(due).await });
    }

    /// Fire best-effort delivery for pre-emptive forecast alerts (budget breach / margin erosion),
    /// after the same per-key cooldown dedup as breaches so a sustained forecast doesn't spam.
    pub(crate) fn notify_forecast(self: &Arc<Self>, alerts: &[ForecastAlert]) {
        if !self.enabled() {
            return;
        }
        let due: Vec<ForecastAlert> = alerts
            .iter()
            .filter(|a| self.should_send_key(&a.dedup_key()))
            .cloned()
            .collect();
        if due.is_empty() {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move { me.deliver_forecast(due).await });
    }

    /// Fire best-effort delivery for relay tasks that just dead-lettered (exhausted their
    /// attempts, or their device vanished past the retry envelope). A task dies at most once, but
    /// the cooldown key still guards against a re-observed transition double-notifying.
    pub(crate) fn notify_relay_dead(self: &Arc<Self>, tasks: &[RelayTask]) {
        if !self.enabled() {
            return;
        }
        let due: Vec<RelayTask> = tasks
            .iter()
            .filter(|t| self.should_send_key(&format!("relay-dead:{}", t.id)))
            .cloned()
            .collect();
        if due.is_empty() {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move { me.deliver_relay_dead(due).await });
    }

    /// True if this breach is outside its cooldown (and records the send time).
    fn should_send(&self, b: &LimitStatus) -> bool {
        self.should_send_key(&format!("{}:{:?}:{:?}", b.project_id, b.metric, b.window))
    }

    /// Cooldown gate keyed by an arbitrary dedup string (records the send time on success).
    fn should_send_key(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut map = self.last_sent.lock().unwrap();
        match map.get(key) {
            Some(t) if now.duration_since(*t) < self.config.cooldown => false,
            _ => {
                map.insert(key.to_string(), now);
                true
            }
        }
    }

    async fn deliver(&self, breaches: Vec<LimitStatus>) {
        for b in &breaches {
            let msg = message(b);
            if let Some(url) = &self.config.webhook {
                self.post_webhook(url, &msg, b).await;
            }
            if let Some(url) = &self.config.ntfy {
                self.post_ntfy(url, &msg).await;
            }
        }
    }

    async fn post_webhook(&self, url: &str, msg: &str, b: &LimitStatus) {
        // `text` (Slack) + `content` (Discord) + structured fields (custom receivers).
        let body = serde_json::json!({
            "event": "limit_breach", "text": msg, "content": msg, "breach": b,
        });
        match self.http.post(url).json(&body).send().await {
            Ok(r) if !r.status().is_success() => eprintln!("[alert] webhook -> HTTP {}", r.status()),
            Err(e) => eprintln!("[alert] webhook error: {e}"),
            _ => {}
        }
    }

    async fn post_ntfy(&self, url: &str, msg: &str) {
        self.post_ntfy_titled(url, "LightTrack limit breach", msg).await
    }

    async fn post_ntfy_titled(&self, url: &str, title: &str, msg: &str) {
        let req = self
            .http
            .post(url)
            .header("Title", title)
            .header("Tags", "warning")
            .header("Priority", "high")
            .body(msg.to_string());
        match req.send().await {
            Ok(r) if !r.status().is_success() => eprintln!("[alert] ntfy -> HTTP {}", r.status()),
            Err(e) => eprintln!("[alert] ntfy error: {e}"),
            _ => {}
        }
    }

    async fn deliver_relay_dead(&self, tasks: Vec<RelayTask>) {
        for t in &tasks {
            let msg = format!(
                "LightTrack alert: relay task '{}' ({}) in project '{}' dead-lettered after {} \
                 attempt(s) — {}",
                t.id,
                t.action_type,
                t.project_id,
                t.attempts,
                t.error.as_deref().unwrap_or("no error recorded"),
            );
            if let Some(url) = &self.config.webhook {
                // `text` (Slack) + `content` (Discord) + a trimmed task (custom receivers) —
                // not the full row: payload/result can be large and may carry app data.
                let body = serde_json::json!({
                    "event": "relay_task_dead", "text": msg, "content": msg,
                    "task": {
                        "id": t.id, "project_id": t.project_id, "action_type": t.action_type,
                        "source": t.source, "attempts": t.attempts, "error": t.error,
                    },
                });
                match self.http.post(url).json(&body).send().await {
                    Ok(r) if !r.status().is_success() => {
                        eprintln!("[alert] relay webhook -> HTTP {}", r.status())
                    }
                    Err(e) => eprintln!("[alert] relay webhook error: {e}"),
                    _ => {}
                }
            }
            if let Some(url) = &self.config.ntfy {
                self.post_ntfy_titled(url, "LightTrack relay task dead", &msg).await;
            }
        }
    }

    async fn deliver_forecast(&self, alerts: Vec<ForecastAlert>) {
        for a in &alerts {
            if let Some(url) = &self.config.webhook {
                // `text` (Slack) + `content` (Discord) + the structured forecast (custom receivers).
                let body = serde_json::json!({
                    "event": "forecast_alert", "text": a.message, "content": a.message, "forecast": a,
                });
                match self.http.post(url).json(&body).send().await {
                    Ok(r) if !r.status().is_success() => {
                        eprintln!("[alert] forecast webhook -> HTTP {}", r.status())
                    }
                    Err(e) => eprintln!("[alert] forecast webhook error: {e}"),
                    _ => {}
                }
            }
            if let Some(url) = &self.config.ntfy {
                self.post_ntfy_titled(url, "LightTrack forecast", &a.message).await;
            }
        }
    }
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

fn message(b: &LimitStatus) -> String {
    format!(
        "LightTrack alert: project '{}' breached {:?}/{:?} limit — current {:.4} >= threshold {:.4} \
         ({:.0}% of limit), action={:?}",
        b.project_id, b.metric, b.window, b.current, b.threshold, b.ratio * 100.0, b.action
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_core::{LimitAction, LimitMetric, LimitWindow};

    fn alerter(cooldown_secs: u64) -> Alerter {
        Alerter {
            config: AlertConfig { webhook: Some("x".into()), ntfy: None, cooldown: Duration::from_secs(cooldown_secs) },
            http: reqwest::Client::new(),
            last_sent: Mutex::new(HashMap::new()),
        }
    }

    fn breach(project: &str) -> LimitStatus {
        LimitStatus {
            rule_id: "r1".into(),
            project_id: project.into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Hour,
            action: LimitAction::Alert,
            current: 2.0,
            threshold: 1.0,
            breached: true,
            ratio: 2.0,
        }
    }

    #[test]
    fn dedup_within_cooldown() {
        let a = alerter(3600);
        let b = breach("p1");
        assert!(a.should_send(&b)); // first send
        assert!(!a.should_send(&b)); // suppressed within cooldown
        assert!(a.should_send(&breach("p2"))); // different key still sends
    }

    #[test]
    fn zero_cooldown_always_sends() {
        let a = alerter(0);
        let b = breach("p1");
        assert!(a.should_send(&b));
        assert!(a.should_send(&b));
    }
}
