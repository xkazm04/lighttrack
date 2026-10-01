//! The two rolling detectors: an error spike, and a quality regression.
//!
//! Both stay **in process memory**, and that is a decision rather than a leftover. They are not
//! facts about the deployment — they are questions about *this replica's recent traffic* ("have I
//! seen five failures in five minutes"), answered from a bounded window. Persisting them would buy
//! nothing an operator can point at, and would put a store write on the ingest path for every
//! failed call. What *is* a fact — the alert that results — goes to the ledger like every other.

use std::collections::VecDeque;
use std::time::Instant;

use super::Alerter;

/// A detected burst of failures for one project — the payload of an error-spike alert.
#[derive(Clone)]
pub(crate) struct ErrorSpike {
    pub(crate) project_id: String,
    pub(crate) count: u32,
    pub(crate) window_secs: u64,
    pub(crate) model: String,
    pub(crate) status: String,
    pub(crate) error: Option<String>,
    /// The PRODUCER's verdict, carried through rather than re-derived downstream. `unknown` when the
    /// SDK did not send one — which is the signal the responder needs to know it must fall back to
    /// reading the message, instead of silently doing so for every event.
    pub(crate) failure_class: lighttrack_core::FailureClass,
}

/// A detected quality regression: the recent mean score for one (project, rubric) has fallen well
/// below its baseline mean.
#[derive(Clone)]
pub(crate) struct ScoreDrop {
    pub(crate) project_id: String,
    pub(crate) rubric: String,
    pub(crate) recent_avg: f64,
    pub(crate) baseline_avg: f64,
    pub(crate) drop_pct: f64,
    pub(crate) samples: usize,
    pub(crate) scored_by: String,
}

impl Alerter {
    /// Push `now` into the project's rolling error window, evict entries older than the window, and
    /// return the current count. Takes an explicit `now` so it is unit-testable.
    pub(super) fn note_error(&self, project: &str, now: Instant) -> u32 {
        let mut map = self.error_windows.lock().unwrap_or_else(|p| p.into_inner());
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

    /// Push a normalized score into the (project, rubric) window (capped at `score_window`) and,
    /// once there are enough samples, return `(recent_mean, baseline_mean, samples)` when the recent
    /// tail has regressed past the drop threshold. No I/O, so it is unit-testable.
    pub(super) fn note_score(&self, key: &str, normalized: f64) -> Option<(f64, f64, usize)> {
        let mut map = self.score_windows.lock().unwrap_or_else(|p| p.into_inner());
        let dq: &mut VecDeque<f64> = map.entry(key.to_string()).or_default();
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
}

/// The window the spike actually used. Whole minutes stay `5m`; every other length is seconds.
/// Integer minutes with a floor of one reported a 30s window as `1m`.
pub(super) fn window_phrase(secs: u64) -> String {
    if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// A window under 6 never has a tail of 3 and a baseline of 3. A minimum above the window never
/// fills. Both used to leave the startup banner claiming score-drop was on.
const SCORE_SAMPLE_FLOOR: usize = 4;
const SCORE_WINDOW_FLOOR: usize = 6;

pub(super) fn score_limits_from(window_raw: u64, min_raw: u64) -> (usize, usize) {
    let asked_window = (window_raw as usize).max(SCORE_SAMPLE_FLOOR);
    let asked_min = (min_raw as usize).max(SCORE_SAMPLE_FLOOR);
    let window = asked_window.max(SCORE_WINDOW_FLOOR);
    let min_samples = asked_min.min(window);
    if window != asked_window || min_samples != asked_min {
        tracing::warn!(
            window = window_raw,
            min_samples = min_raw,
            using_window = window,
            using_min = min_samples,
            "score-drop settings can never trip; using a window the detector can fire"
        );
    }
    (window, min_samples)
}

pub(super) fn score_limits() -> (usize, usize) {
    score_limits_from(
        super::env_u64("LIGHTTRACK_ALERT_SCORE_WINDOW", 20),
        super::env_u64("LIGHTTRACK_ALERT_SCORE_MIN_SAMPLES", 8),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spike(window_secs: u64) -> ErrorSpike {
        ErrorSpike {
            project_id: "p1".into(),
            count: 6,
            window_secs,
            model: "gpt-4o".into(),
            status: "error".into(),
            error: Some("boom".into()),
            failure_class: lighttrack_core::FailureClass::Unknown,
        }
    }

    /// The human sentence is what an on-call reads. Integer minutes with a floor of 1 reported a
    /// 90s window as "1m" and a 30s window as "1m" — both longer than the window that actually fired.
    #[test]
    fn the_spike_message_names_the_window_that_fired() {
        let text = |secs| {
            super::super::compose::error_spike(&spike(secs)).payload["text"]
                .as_str()
                .unwrap_or("")
                .to_string()
        };
        let short = text(30);
        let partial = text(90);
        let whole = text(300);
        assert!(
            short.contains("30s"),
            "a 30s window must not be rounded up to a minute: {short}"
        );
        assert!(
            partial.contains("90s") || partial.contains("1m30s"),
            "a 90s window is not one minute: {partial}"
        );
        assert!(
            whole.contains("5m"),
            "a whole number of minutes stays in minutes: {whole}"
        );
    }

    fn banner_alerter(webhook: Option<&str>, bench: Option<&str>) -> super::super::Alerter {
        use std::collections::HashMap;
        use std::sync::{Mutex, OnceLock};
        use std::time::Duration;
        super::super::Alerter {
            config: super::super::AlertConfig {
                webhook: webhook.map(str::to_string),
                bench_webhook: bench.map(str::to_string),
                ntfy: None,
                resend: None,
                webhook_key: None,
                cooldown: Duration::from_secs(3600),
                error_threshold: 5,
                error_window: Duration::from_secs(300),
                score_window: 20,
                score_min_samples: 8,
                score_drop: 0.15,
                dev_destinations: false,
            },
            http: super::super::http_client(),
            last_sent: Mutex::new(HashMap::new()),
            error_windows: Mutex::new(HashMap::new()),
            score_windows: Mutex::new(HashMap::new()),
            store: OnceLock::new(),
        }
    }

    #[test]
    fn a_distinct_bench_webhook_shows_on_the_startup_banner() {
        let both = banner_alerter(Some("https://hook.test/x"), Some("https://ci.test/runs"));
        assert!(
            both.describe().contains("bench-webhook"),
            "a second receiver must be visible at startup: {}",
            both.describe()
        );
        let same = banner_alerter(Some("https://hook.test/x"), Some("https://hook.test/x"));
        assert!(
            !same.describe().contains("bench-webhook"),
            "the fallback is the same receiver, not a second channel: {}",
            same.describe()
        );
        let only = banner_alerter(None, Some("https://ci.test/runs"));
        assert!(
            only.describe().contains("bench-webhook"),
            "a bench-only deployment must not look like alerting is off: {}",
            only.describe()
        );
    }

    fn score_alerter(window: usize, min_samples: usize) -> super::super::Alerter {
        let mut alerter = banner_alerter(None, None);
        alerter.config.score_window = window;
        alerter.config.score_min_samples = min_samples;
        alerter
    }

    fn drop_can_trip(window: usize, min_samples: usize) -> bool {
        if window < 6 {
            return false;
        }
        let alerter = score_alerter(window, min_samples);
        let recent_k = (window / 4).max(3);
        let base_n = window - recent_k;
        let mut tripped = false;
        for _ in 0..base_n {
            tripped |= alerter.note_score("p:r", 1.0).is_some();
        }
        for _ in 0..recent_k {
            tripped |= alerter.note_score("p:r", 0.0).is_some();
        }
        tripped
    }

    #[test]
    fn a_score_minimum_above_the_window_can_still_trip() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let (window, min_samples) =
            tracing::subscriber::with_default(WarnCapture(std::sync::Arc::clone(&seen)), || {
                super::score_limits_from(4, 100)
            });
        assert!(
            drop_can_trip(window, min_samples),
            "resolved window={window} min={min_samples} still cannot fire"
        );
        let logged = seen.lock().expect("warn lock").clone();
        assert!(
            logged.contains("100"),
            "an impossible score-drop setting must be said out loud: {logged}"
        );

        let quiet = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let defaults =
            tracing::subscriber::with_default(WarnCapture(std::sync::Arc::clone(&quiet)), || {
                super::score_limits_from(20, 8)
            });
        assert_eq!(defaults, (20, 8));
        assert!(
            quiet.lock().expect("warn lock").is_empty(),
            "the defaults already trip and must not warn"
        );
    }

    struct WarnCapture(std::sync::Arc<std::sync::Mutex<String>>);
    impl tracing::Subscriber for WarnCapture {
        fn enabled(&self, meta: &tracing::Metadata<'_>) -> bool {
            *meta.level() <= tracing::Level::WARN
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            event.record(&mut FieldDump(&self.0));
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    struct FieldDump<'a>(&'a std::sync::Mutex<String>);
    impl tracing::field::Visit for FieldDump<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            let mut buf = self.0.lock().expect("warn lock");
            use std::fmt::Write as _;
            let _ = write!(buf, "{}={value:?};", field.name());
        }
    }
}
