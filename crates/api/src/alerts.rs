//! Alert orchestration: cooldown-deduped fan-out of breaches / forecasts / dead relay tasks /
//! error-spikes to webhook + ntfy + email (Resend). This module owns config, the cooldown gate, and
//! the rolling per-project error window; the actual HTTP transport lives in [`channels`].
//!
//! Delivery is **best-effort** and happens **off the request path** (a spawned task), so a slow or
//! down alert sink never delays or fails ingest. Alerts are **deduplicated** per logical key with a
//! cooldown, so a sustained condition (which re-trips on every ingest until the window clears)
//! doesn't spam the channel.
//!
//! Config is server-global via env (per-project routing would need schema/Store changes — payloads
//! carry `project_id` so a single receiver can route):
//!   LIGHTTRACK_ALERT_WEBHOOK              POST a JSON body (Slack/Discord/custom)
//!   LIGHTTRACK_ALERT_NTFY                 POST a text body to an ntfy topic URL
//!   LIGHTTRACK_ALERT_RESEND_KEY           Resend API key — enables email delivery
//!   LIGHTTRACK_ALERT_EMAIL_TO             comma-separated recipient(s) (required for email)
//!   LIGHTTRACK_ALERT_EMAIL_FROM           sender (default onboarding@resend.dev — Resend's shared
//!                                         test sender; a real domain must be verified in Resend)
//!   LIGHTTRACK_ALERT_COOLDOWN_SECS        re-alert window per dedup key (default 3600)
//!   LIGHTTRACK_ALERT_ERROR_THRESHOLD      failed calls per window that trip an error-spike (default 5)
//!   LIGHTTRACK_ALERT_ERROR_WINDOW_SECS    rolling window for the error-spike counter (default 300)

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lighttrack_core::{LimitStatus, LlmEvent, RelayTask};

use crate::forecast::ForecastAlert;

mod channels;

struct AlertConfig {
    webhook: Option<String>,
    ntfy: Option<String>,
    resend: Option<ResendConfig>,
    cooldown: Duration,
    error_threshold: u32,
    error_window: Duration,
}

struct ResendConfig {
    key: String,
    from: String,
    to: Vec<String>,
}

/// A detected burst of failures for one project — the payload of an error-spike alert.
#[derive(Clone)]
struct ErrorSpike {
    project_id: String,
    count: u32,
    window_secs: u64,
    model: String,
    status: String,
    error: Option<String>,
}

pub(crate) struct Alerter {
    config: AlertConfig,
    http: reqwest::Client,
    last_sent: Mutex<HashMap<String, Instant>>,
    error_windows: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl Alerter {
    pub(crate) fn from_env() -> Self {
        Self {
            config: AlertConfig {
                webhook: env_opt("LIGHTTRACK_ALERT_WEBHOOK"),
                ntfy: env_opt("LIGHTTRACK_ALERT_NTFY"),
                resend: ResendConfig::from_env(),
                cooldown: Duration::from_secs(env_u64("LIGHTTRACK_ALERT_COOLDOWN_SECS", 3600)),
                error_threshold: (env_u64("LIGHTTRACK_ALERT_ERROR_THRESHOLD", 5) as u32).max(1),
                error_window: Duration::from_secs(env_u64("LIGHTTRACK_ALERT_ERROR_WINDOW_SECS", 300)),
            },
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            last_sent: Mutex::new(HashMap::new()),
            error_windows: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.config.webhook.is_some() || self.config.ntfy.is_some() || self.config.resend.is_some()
    }

    /// One-line summary for the startup banner.
    pub(crate) fn describe(&self) -> String {
        if !self.enabled() {
            return "off".to_string();
        }
        let mut chans = Vec::new();
        if self.config.webhook.is_some() {
            chans.push("webhook".to_string());
        }
        if self.config.ntfy.is_some() {
            chans.push("ntfy".to_string());
        }
        if let Some(r) = &self.config.resend {
            chans.push(format!("resend({})", r.to.len()));
        }
        format!(
            "{} (cooldown {}s, error-spike >={}/{}s)",
            chans.join("+"),
            self.config.cooldown.as_secs(),
            self.config.error_threshold,
            self.config.error_window.as_secs(),
        )
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
        tokio::spawn(async move { channels::deliver_breaches(&me.config, &me.http, &due).await });
    }

    /// Pre-emptive forecast alerts (budget breach / margin erosion), deduped like breaches.
    pub(crate) fn notify_forecast(self: &Arc<Self>, alerts: &[ForecastAlert]) {
        if !self.enabled() {
            return;
        }
        let due: Vec<ForecastAlert> =
            alerts.iter().filter(|a| self.should_send_key(&a.dedup_key())).cloned().collect();
        if due.is_empty() {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move { channels::deliver_forecast(&me.config, &me.http, &due).await });
    }

    /// Relay tasks that just dead-lettered (exhausted attempts / device vanished).
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
        tokio::spawn(async move { channels::deliver_relay_dead(&me.config, &me.http, &due).await });
    }

    /// Record one non-success ingest event and, if the project crosses its error threshold within the
    /// rolling window, fire a (cooldown-deduped) error-spike alert. O(window) and off the request path
    /// — the delivery itself is spawned. No-op when no channel is configured.
    pub(crate) fn record_error(self: &Arc<Self>, ev: &LlmEvent) {
        if !self.enabled() {
            return;
        }
        let count = self.note_error(&ev.project_id, Instant::now());
        if count < self.config.error_threshold {
            return;
        }
        if !self.should_send_key(&format!("error-spike:{}", ev.project_id)) {
            return;
        }
        let spike = ErrorSpike {
            project_id: ev.project_id.clone(),
            count,
            window_secs: self.config.error_window.as_secs(),
            model: ev.model.clone(),
            status: ev.status.as_str().to_string(),
            error: ev.error.clone(),
        };
        let me = Arc::clone(self);
        tokio::spawn(async move { channels::deliver_error_spike(&me.config, &me.http, &spike).await });
    }

    /// Push `now` into the project's rolling error window, evict entries older than the window, and
    /// return the current count. Split out (takes an explicit `now`) so it is unit-testable.
    fn note_error(&self, project: &str, now: Instant) -> u32 {
        let mut map = self.error_windows.lock().unwrap();
        let dq = map.entry(project.to_string()).or_default();
        dq.push_back(now);
        while let Some(front) = dq.front() {
            if now.duration_since(*front) > self.config.error_window {
                dq.pop_front();
            } else {
                break;
            }
        }
        dq.len() as u32
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
}

impl ResendConfig {
    fn from_env() -> Option<Self> {
        let key = env_opt("LIGHTTRACK_ALERT_RESEND_KEY")?;
        let to: Vec<String> = env_opt("LIGHTTRACK_ALERT_EMAIL_TO")?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if to.is_empty() {
            return None;
        }
        let from = env_opt("LIGHTTRACK_ALERT_EMAIL_FROM")
            .unwrap_or_else(|| "onboarding@resend.dev".to_string());
        Some(ResendConfig { key, from, to })
    }
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_core::{LimitAction, LimitMetric, LimitWindow};

    fn alerter(cooldown_secs: u64) -> Alerter {
        cfg_alerter(cooldown_secs, 5, 300)
    }

    fn cfg_alerter(cooldown_secs: u64, error_threshold: u32, error_window_secs: u64) -> Alerter {
        Alerter {
            config: AlertConfig {
                webhook: Some("x".into()),
                ntfy: None,
                resend: None,
                cooldown: Duration::from_secs(cooldown_secs),
                error_threshold,
                error_window: Duration::from_secs(error_window_secs),
            },
            http: reqwest::Client::new(),
            last_sent: Mutex::new(HashMap::new()),
            error_windows: Mutex::new(HashMap::new()),
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

    #[test]
    fn error_window_counts_and_evicts() {
        let a = cfg_alerter(3600, 3, 60);
        let base = Instant::now();
        assert_eq!(a.note_error("p", base), 1);
        assert_eq!(a.note_error("p", base + Duration::from_secs(30)), 2);
        // `base` is now 90s old (> 60s window) → evicted; `base+30` (60s old) kept; +new = 2.
        assert_eq!(a.note_error("p", base + Duration::from_secs(90)), 2);
        // A different project keeps its own independent window.
        assert_eq!(a.note_error("q", base + Duration::from_secs(90)), 1);
    }
}
