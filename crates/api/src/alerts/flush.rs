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
//! Off by default (`LIGHTTRACK_ALERT_REJECTION_FLUSH_SECS=0`) so a deployment that does not want the
//! extra rows does not get them.

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
    let secs = std::env::var(ENV_INTERVAL)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_INTERVAL_SECS);
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
    let deltas = state.rejections.take_deltas(Utc::now());
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
        .map(|(p, buckets)| compose::ingest_rejected(p, buckets))
        .collect();
    Arc::clone(&state.alerts).fire(alerts).await;
}
