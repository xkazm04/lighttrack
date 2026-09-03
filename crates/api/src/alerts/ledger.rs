//! The durable half: admit-or-suppress, fan out, record every outcome.
//!
//! The order matters. The row is written **before** any delivery is attempted, because the row is
//! what makes the decision: `Store::insert_alert_dedup` is one atomic step, so of two replicas that
//! both decided the same cap had breached, exactly one gets `Admitted` and does the sending. Then
//! each channel's outcome is appended to that row, so `GET /v1/alerts` ([`super::read`]) can answer
//! "you were told, here, at this time, and the webhook returned 503".
//!
//! When no store is attached (a unit test), admission falls back to the in-process cooldown map and
//! delivery still happens — the pre-ledger behaviour, so nothing that used to alert stops alerting.

use std::collections::HashMap;
use std::sync::Arc;

use lighttrack_core::{Alert, LimitStatus, LlmEvent, RelayTask, Score};
use lighttrack_store::AlertAdmission;

use super::{attribution, channels, compose, detectors, Alerter};
use crate::error::ApiError;
use crate::forecast_alerts::ForecastAlert;
use crate::state::spawn_db;

impl Alerter {
    /// Fire best-effort delivery for the given breaches (after the in-process pre-filter). Returns
    /// immediately; admission and HTTP happen on a spawned task.
    pub(crate) fn notify(
        self: &Arc<Self>,
        breaches: &[LimitStatus],
        rejections: &HashMap<String, u64>,
    ) {
        if !self.enabled() {
            return;
        }
        let due: Vec<LimitStatus> = breaches
            .iter()
            .filter(|b| self.should_send(b))
            .cloned()
            .collect();
        if due.is_empty() {
            return;
        }
        let counts: HashMap<String, u64> = due
            .iter()
            .filter_map(|b| {
                let k = self.dedup_key(b);
                rejections.get(&k).map(|c| (k, *c))
            })
            .collect();
        let me = Arc::clone(self);
        tokio::spawn(async move {
            // Attribute the spend inside the delivery task (off the ingest path), best-effort.
            let attributions = me.attribute(&due).await;
            let alerts: Vec<Alert> = due
                .iter()
                .map(|b| {
                    let key = me.dedup_key(b);
                    compose::breach(b, counts.get(&key), attributions.get(&b.alert_key()), key)
                })
                .collect();
            me.fire(alerts).await;
        });
    }

    /// Soft-warning alerts for rules that crossed `warn_at` without breaching. Deduped on a key
    /// *distinct* from the breach key (`warn:…`), so an approaching-limit warning never suppresses
    /// the later breach alert.
    pub(crate) fn notify_warnings(self: &Arc<Self>, warnings: &[LimitStatus]) {
        if !self.enabled() {
            return;
        }
        let due: Vec<Alert> = warnings
            .iter()
            .filter(|w| w.warning)
            .filter_map(|w| {
                let key = self.warn_key(w);
                self.should_send_key(&key).then(|| compose::warning(w, key))
            })
            .collect();
        self.spawn_fire(due);
    }

    /// Pre-emptive forecast alerts (budget breach / margin erosion), deduped like breaches.
    pub(crate) fn notify_forecast(self: &Arc<Self>, alerts: &[ForecastAlert]) {
        if !self.enabled() {
            return;
        }
        let due: Vec<Alert> = alerts
            .iter()
            .filter(|a| self.should_send_key(&a.dedup_key()))
            .map(compose::forecast)
            .collect();
        self.spawn_fire(due);
    }

    /// Relay tasks that just dead-lettered (attempts exhausted / device vanished).
    pub(crate) fn notify_relay_dead(self: &Arc<Self>, tasks: &[RelayTask]) {
        if !self.enabled() {
            return;
        }
        let due: Vec<Alert> = tasks
            .iter()
            .filter(|t| self.should_send_key(&format!("relay-dead:{}", t.id)))
            .map(compose::relay_dead)
            .collect();
        self.spawn_fire(due);
    }

    /// A finished benchmark run. Still honours the dedicated `LIGHTTRACK_BENCH_WEBHOOK`: a CI
    /// receiver subscribed to run completions specifically keeps getting them, and the run is now
    /// also an alert row like everything else.
    pub(crate) fn notify_bench_run(self: &Arc<Self>, run: compose::BenchRunAlert) {
        let alert = compose::bench_run(&run);
        if !self.should_send_key(&alert.dedup_key) {
            return;
        }
        self.spawn_fire(vec![alert]);
    }

    /// Record one non-success ingest event and, if the project crosses its error threshold within
    /// the rolling window, fire a deduped error-spike alert. Off the request path — the delivery is
    /// spawned.
    pub(crate) fn record_error(self: &Arc<Self>, ev: &LlmEvent) {
        if !self.enabled() {
            return;
        }
        let count = self.note_error(&ev.project_id, std::time::Instant::now());
        if count < self.config.error_threshold {
            return;
        }
        if !self.should_send_key(&format!("error-spike:{}", ev.project_id)) {
            return;
        }
        let spike = detectors::ErrorSpike {
            project_id: ev.project_id.clone(),
            count,
            window_secs: self.config.error_window.as_secs(),
            model: ev.model.clone(),
            status: ev.status.as_str().to_string(),
            error: ev.error.clone(),
            failure_class: ev.failure_class(),
        };
        self.spawn_fire(vec![compose::error_spike(&spike)]);
    }

    /// Record one judge score and, if the recent mean for its (project, rubric) has regressed below
    /// the baseline by the configured fraction, fire a deduped `score_drop` alert.
    pub(crate) fn record_score(self: &Arc<Self>, s: &Score) {
        if !self.enabled() || s.max <= 0.0 {
            return;
        }
        let normalized = (s.value / s.max).clamp(0.0, 1.0);
        // `Score::alert_key`, not the free-text label. Two defects rode that label: a rubric renamed
        // between runs split one window into two (and two rubrics sharing a name merged into one),
        // and a per-case label (`bench:x#case7`) is unique per case — so the window never saw the
        // same key twice and this alert could not fire on a benchmark's case stream at all. Run
        // cases now accumulate under their benchmark; everything else keys on `rubric_id` when the
        // row carries one, and on the label when it does not (so existing windows keep history).
        let key = format!("{}\u{1}{}", s.project_id, s.alert_key());
        let Some((recent, baseline, samples)) = self.note_score(&key, normalized) else {
            return;
        };
        let dedup = format!("score-drop:{key}");
        if !self.should_send_key(&dedup) {
            return;
        }
        let drop = detectors::ScoreDrop {
            project_id: s.project_id.clone(),
            rubric: s.rubric.clone(),
            recent_avg: recent,
            baseline_avg: baseline,
            drop_pct: (baseline - recent) / baseline * 100.0,
            samples,
            scored_by: s.scored_by.clone(),
        };
        self.spawn_fire(vec![compose::score_drop(&drop, dedup)]);
    }

    /// Best-effort top-spender attribution for the breaches being delivered, keyed by
    /// [`LimitStatus::alert_key`].
    ///
    /// The store comes from `AppState` now. It used to be a *second* SQLite handle opened from a
    /// file path resolved by re-deriving the API's backend selection from env — which meant
    /// attribution was `None` on Postgres and Firestore, and that a breach alert on the backend
    /// carrying production traffic never said what had burned the money. Any backend that serves
    /// the windowed cost rollups now attributes; one that does not degrades to no attribution, and
    /// the alert delivers unchanged.
    ///
    /// Runs inside the spawned delivery task (zero cost on the ingest path), with the blocking
    /// store reads on the blocking pool.
    async fn attribute(
        &self,
        breaches: &[LimitStatus],
    ) -> HashMap<String, attribution::Attribution> {
        let Some(store) = self.store() else {
            return HashMap::new();
        };
        let breaches = breaches.to_vec();
        tokio::task::spawn_blocking(move || {
            let now = chrono::Utc::now();
            let mut map = HashMap::new();
            for b in &breaches {
                let attr = attribution::fetch(
                    store.as_ref(),
                    &b.project_id,
                    b.window,
                    now,
                    b.scope.as_ref(),
                );
                if !attr.is_empty() {
                    map.insert(b.alert_key(), attr);
                }
            }
            map
        })
        .await
        .unwrap_or_default()
    }

    fn spawn_fire(self: &Arc<Self>, alerts: Vec<Alert>) {
        if alerts.is_empty() {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move { me.fire(alerts).await });
    }

    /// Admit each alert against the durable gate, then deliver the survivors and record every
    /// outcome. This is the whole contract in one function.
    pub(crate) async fn fire(self: &Arc<Self>, alerts: Vec<Alert>) {
        for alert in alerts {
            match self.admit(&alert).await {
                Ok(AlertAdmission::Admitted) => {}
                Ok(AlertAdmission::Suppressed { fired_at }) => {
                    tracing::debug!(
                        dedup_key = %alert.dedup_key, since = %fired_at,
                        "alert suppressed: the same condition is already live"
                    );
                    continue;
                }
                Err(e) => {
                    // The ledger is unavailable. Deliver anyway: an alerting system that goes
                    // silent because its own audit table is down has failed at the one job it has.
                    tracing::warn!(dedup_key = %alert.dedup_key, error = %e,
                        "alert ledger write failed; delivering without a durable record");
                }
            }
            for c in self.channels_for_alert(&alert).await {
                let d = channels::deliver(self, &c, &alert).await;
                if let Some(store) = self.store() {
                    let id = alert.id.clone();
                    let d2 = d.clone();
                    // Best-effort like everything here, but never silent: a delivery record that
                    // fails to land is the ledger saying "nobody was told" about an alert that was
                    // in fact delivered — the exact lie the ledger exists to end. Record it.
                    if let Err(e) = spawn_db(move || store.mark_delivery(&id, &d2)).await {
                        tracing::warn!(
                            alert_id = %alert.id, channel = %c.id, delivered_ok = d.ok, error = %e,
                            "alert was delivered but its delivery record could not be written"
                        );
                    }
                }
            }
        }
    }

    /// The durable cooldown gate. Falls back to `Admitted` when there is no store or the backend
    /// does not serve the ledger — the in-process pre-filter has already run, so that is exactly
    /// the pre-ledger behaviour rather than a flood.
    async fn admit(&self, alert: &Alert) -> Result<AlertAdmission, ApiError> {
        let Some(store) = self.store() else {
            return Ok(AlertAdmission::Admitted);
        };
        let a = alert.clone();
        let cooldown = self.config.cooldown;
        match spawn_db(move || store.insert_alert_dedup(&a, cooldown)).await {
            Ok(v) => Ok(v),
            Err(e) if e.is_unsupported() => Ok(AlertAdmission::Admitted),
            Err(e) => Err(e),
        }
    }
}
