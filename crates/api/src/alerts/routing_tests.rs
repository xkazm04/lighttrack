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
