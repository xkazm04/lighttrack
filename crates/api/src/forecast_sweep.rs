//! Scheduled delivery of the pre-emptive forecast alerts.
//!
//! The budget-ETA and margin-erosion math was already genuine and unit-tested — but `notify_forecast`
//! was only ever reached from inside the `GET /v1/forecast` handler, so "you breach in 2 days" reached
//! an operator only if they were already looking. This module walks the projects on a timer and
//! delivers the same alerts with **no HTTP request involved**.
//!
//! **Where it runs.** In the API process, not the runner. The runner's `recurrence` sweep is the right
//! shape and this borrows it (a `tokio::spawn`ed loop over `tokio::time::interval`, work on the
//! blocking pool), but not the right host: the alert cooldown map, the alert channel config and the
//! `Store` handle all live in `AppState`, and the runner is an optional companion process — a Cloud
//! Run deployment ships the API alone. Hosting the sweep in the runner would mean the alerts silently
//! don't fire in exactly the deployment that most needs them, plus a second `Alerter` whose cooldowns
//! don't see the handler's.
//!
//! **Off by default.** `LIGHTTRACK_FORECAST_SWEEP_SECS` unset (or `0`) = no sweep, because pull-only
//! stays a supported stance: an instance can be a passive store someone else polls, and turning a
//! self-hosted process into an outbound notifier is a decision, not a default. Nothing is delivered
//! anyway unless an alert channel is configured.
//!
//! **It cannot touch the ingest hot path.** It is a detached task; every store read goes through
//! `spawn_db` (the blocking pool, same as any request); it yields between projects; and a failure on
//! one project is logged and skipped rather than ending the loop. Its only shared mutable state is
//! the `Alerter` cooldown map, which is exactly what makes it not spam.

use std::time::Duration;

use lighttrack_core::MarginDimension;

use crate::forecast::compute_forecast;
use crate::forecast_alerts::ForecastAlert;
use crate::state::{spawn_db, AppState};

/// Cadence in seconds; unset or `0` disables the sweep entirely.
const ENV_SECS: &str = "LIGHTTRACK_FORECAST_SWEEP_SECS";
/// How far ahead each scheduled forecast projects (days).
const ENV_HORIZON: &str = "LIGHTTRACK_FORECAST_SWEEP_HORIZON";
/// Trailing days of history each scheduled forecast fits its trend over.
const ENV_LOOKBACK: &str = "LIGHTTRACK_FORECAST_SWEEP_LOOKBACK";

/// Floor on the interval. A forecast is a multi-day projection; sweeping it more often than once a
/// minute only burns store reads, and the alert cooldown (default 1h) would suppress the output
/// anyway.
const MIN_INTERVAL_SECS: u64 = 60;

/// Projection shape when the sweep runs without explicit tuning — the same defaults `GET /v1/forecast`
/// applies to an unparameterized request.
const DEFAULT_HORIZON: u32 = 14;
const DEFAULT_LOOKBACK: u32 = 14;

#[derive(Clone, Copy)]
pub(crate) struct SweepConfig {
    pub(crate) interval: Duration,
    pub(crate) horizon: u32,
    pub(crate) lookback: u32,
}

impl SweepConfig {
    /// `None` when the sweep is off (the default).
    pub(crate) fn from_env() -> Option<Self> {
        let secs = env_u64(ENV_SECS)?;
        if secs == 0 {
            return None;
        }
        Some(SweepConfig {
            interval: Duration::from_secs(secs.max(MIN_INTERVAL_SECS)),
            horizon: env_u64(ENV_HORIZON)
                .unwrap_or(DEFAULT_HORIZON as u64)
                .clamp(1, 90) as u32,
            lookback: env_u64(ENV_LOOKBACK)
                .unwrap_or(DEFAULT_LOOKBACK as u64)
                .clamp(2, 90) as u32,
        })
    }
}

/// One line for the startup banner, so an operator can see at a glance whether anyone will be told.
pub(crate) fn describe(cfg: Option<SweepConfig>) -> String {
    match cfg {
        None => format!("off (set {ENV_SECS})"),
        Some(c) => format!(
            "every {}s (horizon {}d, lookback {}d)",
            c.interval.as_secs(),
            c.horizon,
            c.lookback
        ),
    }
}

/// Start the sweep loop as a detached task. No-op when the sweep is disabled.
pub(crate) fn spawn(st: AppState, cfg: Option<SweepConfig>) {
    let Some(cfg) = cfg else { return };
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(cfg.interval);
        // A forecast over an empty database says nothing; the first tick is spent, not skipped, so
        // startup stays quiet and the first real sweep lands one interval in.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let n = sweep_once(&st).await;
            if n > 0 {
                tracing::info!(
                    raised = n,
                    "forecast sweep raised pre-emptive alerts (cooldown decides delivery)"
                );
            }
        }
    });
}

/// One pass over every enabled project: compute the forecast and hand its alerts to the shared
/// `Alerter`, which applies the same cooldown/dedup keys the request path uses. Returns how many
/// alerts were **raised** — not how many were delivered; the cooldown decides that, which is why a
/// sustained forecast logs a count every sweep while the channel sees it once. Never panics and never propagates — a broken project must
/// not stop the others, or the loop.
pub(crate) async fn sweep_once(st: &AppState) -> usize {
    let store = st.store.clone();
    let projects = match spawn_db(move || store.list_projects()).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "forecast sweep could not list projects");
            return 0;
        }
    };
    let mut produced = 0;
    for p in projects.iter().filter(|p| p.enabled) {
        match forecast_alerts_for(st, &p.id).await {
            Ok(alerts) if !alerts.is_empty() => {
                produced += alerts.len();
                st.alerts.notify_forecast(&alerts);
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(project_id = %p.id, error = %e, "forecast sweep failed for a project")
            }
        }
        // Be a polite background citizen: hand the runtime back between projects so a hundred-project
        // instance can't monopolize a worker while ingest is waiting.
        tokio::task::yield_now().await;
    }
    produced
}

/// The alerts one project's forecast would raise right now — the same list `GET /v1/forecast` returns
/// in its `alerts` field, produced without a request.
async fn forecast_alerts_for(
    st: &AppState,
    project: &str,
) -> Result<Vec<ForecastAlert>, crate::error::ApiError> {
    // Horizon/lookback are read per project rather than captured at spawn, so the loop and a direct
    // call (test, or a future manual trigger) can never disagree about the shape of the projection.
    let (horizon, lookback) = SweepConfig::from_env()
        .map_or((DEFAULT_HORIZON, DEFAULT_LOOKBACK), |c| {
            (c.horizon, c.lookback)
        });
    Ok(
        compute_forecast(st, project, MarginDimension::Customer, horizon, lookback)
            .await?
            .alerts,
    )
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, Utc};
    use lighttrack_core::{
        new_id, LimitAction, LimitMetric, LimitRule, LimitWindow, LlmEvent, Operation, Status,
        TokenUsage,
    };
    use lighttrack_store::Store;

    use crate::redact::Redactor;
    use crate::tests_ingest::setup;

    fn event(project: &str, days_ago: i64, cost: f64) -> LlmEvent {
        let ts = Utc::now() - ChronoDuration::days(days_ago);
        LlmEvent {
            id: new_id(),
            project_id: project.into(),
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            ts,
            received_at: ts,
            provider: "anthropic".into(),
            model: "claude-haiku-4-5".into(),
            name: None,
            operation: Operation::Chat,
            usage: TokenUsage {
                input: 1000,
                output: 500,
                cached_input: None,
                reasoning: None,
            },
            cost_usd: Some(cost),
            latency_ms: Some(10),
            status: Status::Success,
            error: None,
            input: None,
            output: None,
            tags: vec![],
            source: None,
            metadata: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn a_budget_eta_alert_fires_with_no_request_to_the_forecast_endpoint() {
        let (state, store) = setup(Redactor::off());
        crate::tests_ingest::make_key(&store, "proj-a"); // creates the project

        // Ten days of *rising* daily spend, well under a $60/day cap today but trending into it.
        for d in 0..10 {
            let days_ago = 9 - d;
            let per_day = 1.0 + d as f64 * 4.0;
            store
                .insert_event(&event("proj-a", days_ago, per_day))
                .unwrap();
        }
        store
            .create_limit_rule(&LimitRule {
                id: new_id(),
                project_id: "proj-a".into(),
                metric: LimitMetric::CostUsd,
                window: LimitWindow::Day,
                threshold: 60.0,
                action: LimitAction::Alert,
                enabled: true,
                warn_at: None,
                scope: None,
            })
            .unwrap();

        // No router is built and no request is made anywhere in this test — the sweep is the only
        // thing that runs, and it must still produce the alert.
        let produced = sweep_once(&state).await;
        assert!(
            produced > 0,
            "the scheduled sweep must raise the budget-ETA alert on its own"
        );
    }

    #[tokio::test]
    async fn a_second_sweep_is_suppressed_by_the_existing_cooldown_keys() {
        let (state, store) = setup(Redactor::off());
        crate::tests_ingest::make_key(&store, "proj-a");
        for d in 0..10 {
            store
                .insert_event(&event("proj-a", 9 - d, 1.0 + d as f64 * 4.0))
                .unwrap();
        }
        store
            .create_limit_rule(&LimitRule {
                id: new_id(),
                project_id: "proj-a".into(),
                metric: LimitMetric::CostUsd,
                window: LimitWindow::Day,
                threshold: 60.0,
                action: LimitAction::Alert,
                enabled: true,
                warn_at: None,
                scope: None,
            })
            .unwrap();

        // The sweep hands its alerts to the same gate the request path uses, keyed on
        // `forecast:<project>:<kind>:<subject>` with nothing identifying *how* the forecast was
        // triggered — so a sweep every few minutes cannot turn a sustained forecast into a stream of
        // identical notifications, and enabling the sweep cannot double an operator's volume.
        let alerts = forecast_alerts_for(&state, "proj-a")
            .await
            .ok()
            .expect("forecast computes");
        assert!(!alerts.is_empty());
        for a in &alerts {
            assert!(
                !a.dedup_key().contains("sweep"),
                "the key must not fork by trigger: {}",
                a.dedup_key()
            );
            assert!(
                state.alerts.should_send_key(&a.dedup_key()),
                "first presentation sends"
            );
        }
        for a in &alerts {
            assert!(
                !state.alerts.should_send_key(&a.dedup_key()),
                "a repeat sweep is suppressed within the cooldown: {}",
                a.dedup_key()
            );
        }
        assert!(
            state
                .alerts
                .should_send_key("forecast:proj-a:budget_breach:some-other-rule"),
            "an unrelated key is unaffected"
        );
    }

    #[tokio::test]
    async fn a_quiet_project_produces_nothing_and_a_broken_one_does_not_stop_the_loop() {
        let (state, store) = setup(Redactor::off());
        crate::tests_ingest::make_key(&store, "proj-a");
        // No events, no rules: no trend, no ETA, nothing to say.
        assert_eq!(sweep_once(&state).await, 0);
        // A disabled project is skipped entirely.
        let mut p = store.get_project("proj-a").unwrap().unwrap();
        p.enabled = false;
        store.update_project(&p).unwrap();
        assert_eq!(sweep_once(&state).await, 0);
    }

    #[test]
    fn the_sweep_is_off_unless_explicitly_configured() {
        // Env-driven, and this test process sets nothing: the default stance is pull-only.
        assert!(SweepConfig::from_env().is_none());
        assert!(describe(None).starts_with("off"));
        let cfg = SweepConfig {
            interval: Duration::from_secs(300),
            horizon: 14,
            lookback: 14,
        };
        assert!(describe(Some(cfg)).contains("every 300s"));
    }
}
