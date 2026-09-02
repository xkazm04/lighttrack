//! Alert orchestration: what fired, where it goes, and what happened to it.
//!
//! This module owns configuration and the *entry points* the rest of the API calls
//! (`notify`, `notify_warnings`, `notify_forecast`, `notify_relay_dead`, `notify_bench_run`,
//! `record_error`, `record_score`). Everything else is a neighbour:
//!
//! | module | what it owns |
//! |---|---|
//! | [`compose`] | turning a typed condition into an [`Alert`] row + its human message |
//! | [`ledger`] | the **durable** cooldown gate, fan-out, and the `/v1/alerts` surface |
//! | [`routing`] | which channels an alert goes to, and the channel admin routes |
//! | [`channels`] | the actual HTTP posts, and recording each outcome |
//! | [`sign`] | the `X-LightTrack-Signature` contract |
//! | [`vet`] | which destinations may be fetched at all |
//! | [`detectors`] | the two rolling *rate* detectors (error spike, score drop) |
//! | [`attribution`] | what drove the spend behind a breach |
//!
//! Delivery is **best-effort** and happens **off the request path** (a spawned task), so a slow
//! sink never delays ingest. Deduplication is a store step ([`ledger::admit`]) rather than a
//! process's memory: production is multi-replica, and an in-process map made every replica alert
//! independently and forget everything on restart.
//!
//! The in-memory maps that remain are deliberate. `last_sent` is a **write-through cache** in front
//! of the durable gate — it spares the store a round trip for a condition this replica just
//! alerted on, and the store is still the decider. `error_windows` / `score_windows` are **rate
//! detectors**, not facts: they answer "has this project failed 5 times in the last 5 minutes",
//! which is a question about this replica's recent traffic, and losing them on restart loses
//! nothing an operator can point at.
//!
//! Config is env-global; env destinations are synthesised as **global channels** (see [`routing`]),
//! and per-project routing is added on top with `PUT /v1/projects/:id/alert-channels`:
//!   LIGHTTRACK_ALERT_WEBHOOK              POST a JSON body (Slack/Discord/custom)
//!   LIGHTTRACK_BENCH_WEBHOOK              a `bench_run`-only webhook (falls back to the one above)
//!   LIGHTTRACK_ALERT_NTFY                 POST a text body to an ntfy topic URL
//!   LIGHTTRACK_ALERT_RESEND_KEY           Resend API key — enables email delivery
//!   LIGHTTRACK_ALERT_EMAIL_TO             comma-separated recipient(s) (required for email)
//!   LIGHTTRACK_ALERT_EMAIL_FROM           sender (default onboarding@resend.dev)
//!   LIGHTTRACK_ALERT_WEBHOOK_SECRET       signs the env webhook's deliveries (see [`sign`])
//!   LIGHTTRACK_ALERT_COOLDOWN_SECS        re-alert window per dedup key (default 3600)
//!   LIGHTTRACK_ALERT_ERROR_THRESHOLD      failed calls per window that trip an error-spike (5)
//!   LIGHTTRACK_ALERT_ERROR_WINDOW_SECS    rolling window for the error-spike counter (300)
//!   LIGHTTRACK_ALERT_SCORE_WINDOW         per-(project,rubric) score window (default 20)
//!   LIGHTTRACK_ALERT_SCORE_MIN_SAMPLES    min scores before a regression can trip (default 8)
//!   LIGHTTRACK_ALERT_SCORE_DROP           recent-vs-baseline mean drop that trips score_drop (0.15)
//!   LIGHTTRACK_ALERT_ALLOW_LOOPBACK       dev only: permit http:// loopback destinations
//!   LIGHTTRACK_ALERT_REJECTION_FLUSH_SECS how often the rejection ledger is flushed (0 = off)

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use lighttrack_core::LimitStatus;
use lighttrack_store::Store;

pub(crate) mod attribution;
mod channels;
mod compose;
mod detectors;
pub(crate) mod flush;
mod ledger;
pub(crate) mod read;
pub(crate) mod routing;
mod sign;
mod vet;

#[cfg(test)]
mod tests;

pub(crate) use compose::BenchRunAlert;

pub(crate) struct AlertConfig {
    pub(crate) webhook: Option<String>,
    /// Dedicated benchmark-completion webhook (`LIGHTTRACK_BENCH_WEBHOOK`); falls back to the
    /// general alert webhook so a single receiver can serve both.
    pub(crate) bench_webhook: Option<String>,
    pub(crate) ntfy: Option<String>,
    pub(crate) resend: Option<ResendConfig>,
    /// Signs the env webhook's deliveries. Stored as the derived key (see [`sign`]).
    pub(crate) webhook_key: Option<String>,
    pub(crate) cooldown: Duration,
    pub(crate) error_threshold: u32,
    pub(crate) error_window: Duration,
    pub(crate) score_window: usize,
    pub(crate) score_min_samples: usize,
    pub(crate) score_drop: f64,
    /// Dev-mode destination relaxation (`http://localhost`). See [`vet`].
    pub(crate) dev_destinations: bool,
}

pub(crate) struct ResendConfig {
    pub(crate) key: String,
    pub(crate) from: String,
    pub(crate) to: Vec<String>,
}

pub(crate) struct Alerter {
    pub(crate) config: AlertConfig,
    pub(crate) http: reqwest::Client,
    /// Write-through cache in front of the durable gate — never the decider. See the module docs.
    last_sent: Mutex<HashMap<String, Instant>>,
    /// Rate detectors, not facts: intentionally per-replica and lost on restart.
    error_windows: Mutex<HashMap<String, VecDeque<Instant>>>,
    score_windows: Mutex<HashMap<String, VecDeque<f64>>>,
    /// The store the ledger and the routing table live in, attached once at startup. `None` in a
    /// unit test that never wired one, where delivery degrades to the pre-ledger behaviour.
    store: OnceLock<Arc<dyn Store + Send + Sync>>,
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
                webhook_key: env_opt("LIGHTTRACK_ALERT_WEBHOOK_SECRET")
                    .map(|s| sign::derive_key(&s)),
                cooldown: Duration::from_secs(env_u64("LIGHTTRACK_ALERT_COOLDOWN_SECS", 3600)),
                error_threshold: (env_u64("LIGHTTRACK_ALERT_ERROR_THRESHOLD", 5) as u32).max(1),
                error_window: Duration::from_secs(env_u64(
                    "LIGHTTRACK_ALERT_ERROR_WINDOW_SECS",
                    300,
                )),
                score_window: (env_u64("LIGHTTRACK_ALERT_SCORE_WINDOW", 20) as usize).max(4),
                score_min_samples: (env_u64("LIGHTTRACK_ALERT_SCORE_MIN_SAMPLES", 8) as usize)
                    .max(4),
                score_drop: env_f64("LIGHTTRACK_ALERT_SCORE_DROP", 0.15),
                dev_destinations: vet::dev_destinations(),
            },
            http: http_client(),
            last_sent: Mutex::new(HashMap::new()),
            error_windows: Mutex::new(HashMap::new()),
            score_windows: Mutex::new(HashMap::new()),
            store: OnceLock::new(),
        }
    }

    /// Hand the alerter the store, once, at startup. Everything durable — the dedup gate, the
    /// delivery record, per-project routing, breach attribution — hangs off this.
    pub(crate) fn attach_store(&self, store: Arc<dyn Store + Send + Sync>) {
        let _ = self.store.set(store);
    }

    pub(crate) fn store(&self) -> Option<Arc<dyn Store + Send + Sync>> {
        self.store.get().cloned()
    }

    /// True when *something* could receive an alert: an env destination, or a stored channel.
    ///
    /// The stored half cannot be answered synchronously (it is a store read), so this stays the
    /// env question and [`ledger::fire`] resolves the real channel set per alert. A deployment
    /// with only per-project channels therefore still alerts — `enabled()` no longer gates it.
    pub(crate) fn enabled(&self) -> bool {
        self.config.webhook.is_some()
            || self.config.ntfy.is_some()
            || self.config.resend.is_some()
            || self.store.get().is_some()
    }

    /// One-line summary for the startup banner.
    pub(crate) fn describe(&self) -> String {
        let mut chans = Vec::new();
        if self.config.webhook.is_some() {
            chans.push(if self.config.webhook_key.is_some() {
                "webhook(signed)".to_string()
            } else {
                "webhook".to_string()
            });
        }
        if self.config.ntfy.is_some() {
            chans.push("ntfy".to_string());
        }
        if let Some(r) = &self.config.resend {
            chans.push(format!("resend({})", r.to.len()));
        }
        if chans.is_empty() && self.config.bench_webhook.is_some() {
            return "env: off (bench-webhook on), ledger: on".to_string();
        }
        if chans.is_empty() {
            return "env: off (per-project channels only), ledger: on".to_string();
        }
        format!(
            "{} (cooldown {}s, error-spike >={}/{}s, score-drop >={:.0}%), ledger: on",
            chans.join("+"),
            self.config.cooldown.as_secs(),
            self.config.error_threshold,
            self.config.error_window.as_secs(),
            self.config.score_drop * 100.0,
        )
    }

    /// True if this key is outside its cooldown *in this process* (and records the check).
    ///
    /// A pre-filter, not the decision: [`ledger::admit`] is what actually admits or suppresses,
    /// because only the store can decide it for every replica at once.
    pub(crate) fn should_send_key(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut map = self.last_sent.lock().unwrap_or_else(|p| p.into_inner());
        match map.get(key) {
            Some(t) if now.duration_since(*t) < self.config.cooldown => false,
            _ => {
                map.insert(key.to_string(), now);
                true
            }
        }
    }

    /// Stable per-breach key (`project:metric:window:scope`) — shared by the cooldown gate and the
    /// rejection ledger so a breach's alert can be matched to its running rejection count.
    pub(crate) fn dedup_key(&self, b: &LimitStatus) -> String {
        b.alert_key()
    }

    /// Cooldown key for a soft warning — the breach key prefixed with `warn:` so the warning and
    /// the eventual breach for the *same* rule track independent cooldowns.
    pub(crate) fn warn_key(&self, b: &LimitStatus) -> String {
        format!("warn:{}", self.dedup_key(b))
    }

    fn should_send(&self, b: &LimitStatus) -> bool {
        self.should_send_key(&self.dedup_key(b))
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

/// The one client every alert delivery goes through.
///
/// A named constructor rather than an inline builder because the test fixture must be able to build
/// the *same* client: with `reqwest::Client::new()` in the tests, the redirect assertion below
/// passed against a client production never uses, which is a test that proves nothing.
///
/// `Policy::none()` is the load-bearing part. `vet` checks the URL we were configured with; a 302
/// from an operator-supplied webhook to `http://169.254.169.254/...` is a *different* URL, fetched
/// after every check has already run. See [`vet`].
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
