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
//!   LIGHTTRACK_ALERT_SCORE_WINDOW         per-(project,rubric) score window for regression (default 20)
//!   LIGHTTRACK_ALERT_SCORE_MIN_SAMPLES    min scores before a regression can trip (default 8)
//!   LIGHTTRACK_ALERT_SCORE_DROP           recent-vs-baseline mean drop that trips score_drop (0.15)

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lighttrack_core::{LimitStatus, LlmEvent, RelayTask, Score};

use crate::forecast::ForecastAlert;

mod channels;

struct AlertConfig {
    webhook: Option<String>,
    /// Dedicated benchmark-completion webhook (`LIGHTTRACK_BENCH_WEBHOOK`); falls back to the general
    /// alert webhook so a single receiver can serve both. Independent of the other alert channels.
    bench_webhook: Option<String>,
    ntfy: Option<String>,
    resend: Option<ResendConfig>,
    cooldown: Duration,
    error_threshold: u32,
    error_window: Duration,
    /// Rolling per-(project, rubric) score window size for quality-regression detection.
    score_window: usize,
    /// Minimum scores in the window before a regression can trip.
    score_min_samples: usize,
    /// Relative drop of the recent mean vs the baseline mean that trips a `score_drop` (e.g. 0.15).
    score_drop: f64,
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

/// A detected quality regression: the recent mean score for one (project, rubric) has fallen well
/// below its baseline mean.
#[derive(Clone)]
struct ScoreDrop {
    project_id: String,
    rubric: String,
    recent_avg: f64,
    baseline_avg: f64,
    drop_pct: f64,
    samples: usize,
    scored_by: String,
}

/// A finished benchmark run — the payload of a completion webhook (Direction: CI gate contract).
#[derive(Clone)]
pub(crate) struct BenchRunAlert {
    pub(crate) benchmark: String,
    pub(crate) run_id: String,
    pub(crate) status: String,
    pub(crate) mean: Option<f64>,
    pub(crate) baseline: Option<f64>,
}

pub(crate) struct Alerter {
    config: AlertConfig,
    http: reqwest::Client,
    last_sent: Mutex<HashMap<String, Instant>>,
    error_windows: Mutex<HashMap<String, VecDeque<Instant>>>,
    score_windows: Mutex<HashMap<String, VecDeque<f64>>>,
}

impl Alerter {
    pub(crate) fn from_env() -> Self {
        Self {
            config: AlertConfig {
                webhook: env_opt("LIGHTTRACK_ALERT_WEBHOOK"),
                bench_webhook: env_opt("LIGHTTRACK_BENCH_WEBHOOK")
                    .or_else(|| env_opt("LIGHTTRACK_ALERT_WEBHOOK")),
                ntfy: env_opt("LIGHTTRACK_ALERT_NTFY"),
                resend: ResendConfig::from_env(),
                cooldown: Duration::from_secs(env_u64("LIGHTTRACK_ALERT_COOLDOWN_SECS", 3600)),
                error_threshold: (env_u64("LIGHTTRACK_ALERT_ERROR_THRESHOLD", 5) as u32).max(1),
                error_window: Duration::from_secs(env_u64("LIGHTTRACK_ALERT_ERROR_WINDOW_SECS", 300)),
                score_window: (env_u64("LIGHTTRACK_ALERT_SCORE_WINDOW", 20) as usize).max(4),
                score_min_samples: (env_u64("LIGHTTRACK_ALERT_SCORE_MIN_SAMPLES", 8) as usize).max(4),
                score_drop: env_f64("LIGHTTRACK_ALERT_SCORE_DROP", 0.15),
            },
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            last_sent: Mutex::new(HashMap::new()),
            error_windows: Mutex::new(HashMap::new()),
            score_windows: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.config.webhook.is_some() || self.config.ntfy.is_some() || self.config.resend.is_some()
    }

    /// One-line summary for the startup banner.
    pub(crate) fn describe(&self) -> String {
        if !self.enabled() {
            return if self.config.bench_webhook.is_some() {
                "off (bench-webhook on)".to_string()
            } else {
                "off".to_string()
            };
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
            "{} (cooldown {}s, error-spike >={}/{}s, score-drop >={:.0}%)",
            chans.join("+"),
            self.config.cooldown.as_secs(),
            self.config.error_threshold,
            self.config.error_window.as_secs(),
            self.config.score_drop * 100.0,
        )
    }

    /// Fire best-effort delivery for the given breaches (after per-key cooldown dedup). Returns
    /// immediately; the actual HTTP happens on a spawned task. `rejections` maps a breach's dedup key
    /// (`project:metric:window`) to the running count of ingest attempts that rule has rejected, so an
    /// enforcing breach's alert can report how many calls the cap has turned away.
    pub(crate) fn notify(self: &Arc<Self>, breaches: &[LimitStatus], rejections: &HashMap<String, u64>) {
        if !self.enabled() {
            return;
        }
        let due: Vec<LimitStatus> = breaches.iter().filter(|b| self.should_send(b)).cloned().collect();
        if due.is_empty() {
            return;
        }
        // Carry only the counts for the breaches actually being sent.
        let counts: HashMap<String, u64> = due
            .iter()
            .filter_map(|b| {
                let k = self.dedup_key(b);
                rejections.get(&k).map(|c| (k, *c))
            })
            .collect();
        let me = Arc::clone(self);
        tokio::spawn(
            async move { channels::deliver_breaches(&me.config, &me.http, &due, &counts).await },
        );
    }

    /// Fire best-effort **soft-warning** alerts for rules that crossed their `warn_at` fraction
    /// without breaching. Deduped on a key *distinct* from the breach key (`warn:…` vs `…`) and its
    /// own cooldown, so an approaching-limit warning never suppresses the later breach alert (or vice
    /// versa). Warnings are observe-only — they never enforce.
    pub(crate) fn notify_warnings(self: &Arc<Self>, warnings: &[LimitStatus]) {
        if !self.enabled() {
            return;
        }
        let due: Vec<LimitStatus> = warnings
            .iter()
            .filter(|w| w.warning && self.should_send_key(&self.warn_key(w)))
            .cloned()
            .collect();
        if due.is_empty() {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move { channels::deliver_warnings(&me.config, &me.http, &due).await });
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

    /// Fire a best-effort benchmark-completion webhook (off the request path), deduped per
    /// (benchmark, status) within the cooldown so a flapping benchmark doesn't spam the receiver.
    /// No-op unless a bench webhook is configured. Independent of the other alert channels.
    pub(crate) fn notify_bench_run(self: &Arc<Self>, run: BenchRunAlert) {
        let Some(url) = self.config.bench_webhook.clone() else {
            return;
        };
        if !self.should_send_key(&format!("bench-run:{}:{}", run.benchmark, run.status)) {
            return;
        }
        let http = self.http.clone();
        tokio::spawn(async move { channels::deliver_bench_run(&http, &url, &run).await });
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

    /// Record one judge score and, if the recent mean for its (project, rubric) has regressed below
    /// the baseline mean by the configured fraction, fire a (cooldown-deduped) `score_drop` alert.
    pub(crate) fn record_score(self: &Arc<Self>, s: &Score) {
        if !self.enabled() || s.max <= 0.0 {
            return;
        }
        let normalized = (s.value / s.max).clamp(0.0, 1.0);
        let key = format!("{}\u{1}{}", s.project_id, s.rubric);
        let Some((recent, baseline, samples)) = self.note_score(&key, normalized) else {
            return;
        };
        if !self.should_send_key(&format!("score-drop:{key}")) {
            return;
        }
        let drop = ScoreDrop {
            project_id: s.project_id.clone(),
            rubric: s.rubric.clone(),
            recent_avg: recent,
            baseline_avg: baseline,
            drop_pct: (baseline - recent) / baseline * 100.0,
            samples,
            scored_by: s.scored_by.clone(),
        };
        let me = Arc::clone(self);
        tokio::spawn(async move { channels::deliver_score_drop(&me.config, &me.http, &drop).await });
    }

    /// Push a normalized score into the (project, rubric) window (capped at `score_window`) and, once
    /// there are enough samples, return `(recent_mean, baseline_mean)` when the recent tail has
    /// regressed past the drop threshold. Split out (no I/O) so it is unit-testable.
    fn note_score(&self, key: &str, normalized: f64) -> Option<(f64, f64, usize)> {
        let mut map = self.score_windows.lock().unwrap();
        let dq = map.entry(key.to_string()).or_default();
        dq.push_back(normalized);
        while dq.len() > self.config.score_window {
            dq.pop_front();
        }
        let len = dq.len();
        if len < self.config.score_min_samples {
            return None;
        }
        let recent_k = (len / 4).max(3);
        let base_n = len.checked_sub(recent_k)?;
        if base_n < 3 {
            return None;
        }
        let recent: f64 = dq.iter().skip(base_n).sum::<f64>() / recent_k as f64;
        let baseline: f64 = dq.iter().take(base_n).sum::<f64>() / base_n as f64;
        if baseline <= 0.0 {
            return None;
        }
        if (baseline - recent) / baseline >= self.config.score_drop {
            Some((recent, baseline, len))
        } else {
            None
        }
    }

    /// True if this breach is outside its cooldown (and records the send time).
    fn should_send(&self, b: &LimitStatus) -> bool {
        self.should_send_key(&self.dedup_key(b))
    }

    /// Stable per-breach key (`project:metric:window:scope`) — shared by cooldown dedup and the
    /// rejection ledger so a breach's alert can be matched to its running rejection count, and a
    /// scoped cap doesn't collide with a project-wide one on the same metric+window.
    fn dedup_key(&self, b: &LimitStatus) -> String {
        b.alert_key()
    }

    /// Cooldown key for a soft-warning — the breach key prefixed with `warn:` so the warning and the
    /// eventual breach for the *same* rule track independent cooldowns.
    fn warn_key(&self, b: &LimitStatus) -> String {
        format!("warn:{}", self.dedup_key(b))
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

fn env_f64(key: &str, default: f64) -> f64 {
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
                bench_webhook: None,
                ntfy: None,
                resend: None,
                cooldown: Duration::from_secs(cooldown_secs),
                error_threshold,
                error_window: Duration::from_secs(error_window_secs),
                score_window: 20,
                score_min_samples: 8,
                score_drop: 0.15,
            },
            http: reqwest::Client::new(),
            last_sent: Mutex::new(HashMap::new()),
            error_windows: Mutex::new(HashMap::new()),
            score_windows: Mutex::new(HashMap::new()),
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
            warn_at: None,
            warning: false,
            scope: None,
        }
    }

    fn warning(project: &str) -> LimitStatus {
        LimitStatus {
            rule_id: "r1".into(),
            project_id: project.into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Hour,
            action: LimitAction::Alert,
            current: 0.85,
            threshold: 1.0,
            breached: false,
            ratio: 0.85,
            warn_at: Some(0.8),
            warning: true,
            scope: None,
        }
    }

    #[test]
    fn warning_and_breach_have_independent_cooldowns() {
        let a = alerter(3600);
        let w = warning("p1");
        let b = breach("p1");
        // Same rule: the warning key and the breach key don't collide, so each sends once and the
        // warning never suppresses the breach.
        assert!(a.should_send_key(&a.warn_key(&w)), "warning sends first time");
        assert!(!a.should_send_key(&a.warn_key(&w)), "warning suppressed within cooldown");
        assert!(a.should_send(&b), "breach still sends despite the earlier warning");
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

    #[test]
    fn score_regression_detected() {
        let a = alerter(3600); // default score window 20 / min 8 / drop 0.15
        // A run of good scores establishes the baseline — no regression.
        for _ in 0..12 {
            assert!(a.note_score("p\u{1}helpfulness", 0.9).is_none());
        }
        // The recent tail turning bad trips the regression.
        let mut tripped = false;
        for _ in 0..4 {
            if a.note_score("p\u{1}helpfulness", 0.4).is_some() {
                tripped = true;
            }
        }
        assert!(tripped);
        // A steady-but-low rubric (no baseline-vs-recent gap) does not trip.
        for _ in 0..12 {
            assert!(a.note_score("p\u{1}steady", 0.5).is_none());
        }
    }
}
