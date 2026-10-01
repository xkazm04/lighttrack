use super::*;
use crate::error::{ApiError, ErrorCode};
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

/// `LIGHTTRACK_BENCH_WEBHOOK` on its own must reach a receiver — and when it merely falls back
/// to the general webhook it must not make that receiver hear every run twice.
#[test]
fn the_dedicated_bench_webhook_is_a_bench_run_only_channel_and_never_a_duplicate() {
    let mut only_bench = cfg(None, None);
    only_bench.bench_webhook = Some("https://ci.test/runs".into());
    let ch = env_channels(&only_bench);
    assert_eq!(ch.len(), 1);
    assert_eq!(ch[0].id, ENV_BENCH_WEBHOOK);
    assert!(ch[0].accepts(AlertKind::BenchRun, Severity::Info));
    assert!(
        !ch[0].accepts(AlertKind::LimitBreach, Severity::Critical),
        "a CI receiver subscribed to run completions gets nothing else"
    );

    let mut fallback = cfg(Some("https://hook.test/x"), None);
    fallback.bench_webhook = Some("https://hook.test/x".into());
    assert_eq!(
        env_channels(&fallback).len(),
        1,
        "the fallback is the same receiver, not a second channel"
    );
}

#[test]
fn a_channel_lookup_failure_is_logged_and_an_unsupported_backend_is_quiet() {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let failed =
        tracing::subscriber::with_default(WarnCapture(std::sync::Arc::clone(&seen)), || {
            absorb_stored_channels(Err(ApiError::internal("disk full")))
        });
    assert!(
        failed.is_empty(),
        "a lookup failure still delivers no stored rows"
    );
    let logged = seen.lock().expect("warn lock").clone();
    assert!(
        logged.contains("disk full"),
        "a real store error must be said out loud, not swallowed: {logged}"
    );

    let quiet = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let unsupported =
        tracing::subscriber::with_default(WarnCapture(std::sync::Arc::clone(&quiet)), || {
            absorb_stored_channels(Err(ApiError::new(ErrorCode::Unsupported, "alert routing")))
        });
    assert!(unsupported.is_empty());
    assert!(
        quiet.lock().expect("warn lock").is_empty(),
        "an unsupported backend is a declared gap, not an outage"
    );

    let ok = absorb_stored_channels(Ok(env_channels(&cfg(Some("https://hook.test/x"), None))));
    assert_eq!(ok.len(), 1);
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
