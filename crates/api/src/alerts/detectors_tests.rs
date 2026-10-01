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
