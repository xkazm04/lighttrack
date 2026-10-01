//! Periodic flush of the in-process rejection ledger into `AlertKind::IngestRejected` rows.
//!
//! [`crate::rejections`] counts the ingest attempts a cap turned away, because those calls are
//! deliberately never stored as events — storing them would corrupt the usage and cost rollups every
//! cap is evaluated against. But that made the counter process-local and lost on restart, which is
//! the same "it was only ever in RAM" problem the alert ledger exists to end.
//!
//! So the counter stays where it is (a hot-path `Mutex`, not a store write per rejected call) and
//! its **deltas** are flushed on a timer as alert rows. An alert row and never an event: the row
//! records that a cap turned traffic away, which is a fact about the *limit*, not about a call that
//! happened.
//!
//! On by default, every 900s. Set `LIGHTTRACK_ALERT_REJECTION_FLUSH_SECS=0` to switch it off.
//! A value that is not a number warns and keeps 900, the same rule as the other alert settings.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use super::compose;
use crate::state::AppState;

const ENV_INTERVAL: &str = "LIGHTTRACK_ALERT_REJECTION_FLUSH_SECS";
/// Default cadence. Long enough that a sustained rejection storm produces a handful of rows rather
/// than a stream, short enough that an operator paging through the ledger sees it while it matters.
const DEFAULT_INTERVAL_SECS: u64 = 900;

/// The flush cadence, or `None` when it is switched off.
pub(crate) fn interval() -> Option<Duration> {
    let secs = super::env_u64(ENV_INTERVAL, DEFAULT_INTERVAL_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

pub(crate) fn describe() -> String {
    match interval() {
        Some(d) => format!("every {}s", d.as_secs()),
        None => "off".to_string(),
    }
}

/// Spawn the flush loop. Detached, like the other sweeps: it never shares a task with a request.
pub(crate) fn spawn(state: AppState, every: Option<Duration>) {
    let Some(every) = every else { return };
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; skip it so a restart does not re-report an empty window.
        tick.tick().await;
        loop {
            tick.tick().await;
            flush_once(&state).await;
        }
    });
}

/// One pass: take the deltas since the last flush and fire one alert per project that had any.
pub(crate) async fn flush_once(state: &AppState) {
    let now = Utc::now();
    let deltas = state.rejections.take_deltas(now);
    if deltas.is_empty() {
        return;
    }
    // One alert per project, listing the rules — an operator's unit of attention is the project,
    // not the individual cap.
    let mut by_project: HashMap<String, HashMap<String, (u64, f64)>> = HashMap::new();
    for (project, rule, count, cost) in deltas {
        by_project
            .entry(project)
            .or_default()
            .insert(rule, (count, cost));
    }
    let alerts = by_project
        .iter()
        .map(|(p, buckets)| compose::ingest_rejected(p, buckets, now))
        .collect();
    Arc::clone(&state.alerts).fire(alerts).await;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    /// `AlertKind::IngestRejected` declares Info, and the operator doc says the same. A channel
    /// whose floor is Warning is the on-call page; a periodic rejection tally must not cross it.
    #[test]
    fn a_rejection_flush_stays_informational() {
        let buckets = HashMap::from([("rule".to_string(), (4u64, 1.25f64))]);
        let alert = super::compose::ingest_rejected("p1", &buckets, chrono::Utc::now());
        assert_eq!(
            alert.severity,
            lighttrack_core::Severity::Info,
            "a rejection flush is a tally, not a page"
        );
        assert!(
            !lighttrack_core::AlertChannel {
                min_severity: lighttrack_core::Severity::Warning,
                ..channel_floor()
            }
            .accepts(alert.kind, alert.severity),
            "a warning floor must not receive an informational flush"
        );
    }

    /// `LIGHTTRACK_ALERT_COOLDOWN_SECS=1h` warns and falls back. The flush cadence used to do
    /// neither: `15m` became the default with no line in the log, which reads as "my setting works".
    #[test]
    fn a_malformed_flush_interval_warns_and_keeps_the_default() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let subscriber = WarnCapture(std::sync::Arc::clone(&seen));
        let key = super::ENV_INTERVAL;
        let _guard = EnvVar::set(key, "15m");
        tracing::subscriber::with_default(subscriber, || {
            let got = super::interval();
            assert_eq!(got, Some(std::time::Duration::from_secs(900)));
        });
        let logged = seen.lock().expect("warn lock").clone();
        assert!(
            logged.contains("15m"),
            "a bad flush interval must be said out loud, not treated as unset: {logged}"
        );

        let _zero = EnvVar::set(key, "0");
        assert_eq!(super::interval(), None, "0 switches the flush off");
        let _mins = EnvVar::set(key, "120");
        assert_eq!(super::interval(), Some(std::time::Duration::from_secs(120)));
    }

    struct EnvVar(&'static str);
    impl EnvVar {
        fn set(key: &'static str, value: &str) -> Self {
            std::env::set_var(key, value);
            Self(key)
        }
    }
    impl Drop for EnvVar {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
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

    fn channel_floor() -> lighttrack_core::AlertChannel {
        lighttrack_core::AlertChannel {
            id: "c".into(),
            project_id: None,
            kind: lighttrack_core::ChannelKind::Webhook,
            target: "https://hook.test/x".into(),
            secret_hash: None,
            prev_secret_hash: None,
            min_severity: lighttrack_core::Severity::Info,
            kinds: Vec::new(),
            enabled: true,
            created_at: chrono::Utc::now(),
        }
    }
}
