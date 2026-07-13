//! In-process ledger of *rejected* ingest attempts — the calls a limit rule turned away with a 429.
//!
//! When an enforcing (`Throttle`/`Block`) breach rejects an event, the store never records it (that
//! would corrupt the usage/cost rollups every cap is evaluated against). But an observability tool
//! going blind exactly when limits bite is a product flaw: the breaching calls are the most
//! interesting ones to have counted. This ledger closes that gap **without** persisting a fake event
//! — a `Mutex`'d map keyed by `(project, metric, window)` holding a running count + estimated missed
//! cost, mirroring the [`SeenWebhooks`](crate::idempotency) / alert-cooldown in-memory state.
//!
//! **Best-effort and process-local by design:** it lives in RAM and resets on restart, and entries
//! older than [`TTL_HOURS`] are pruned. It is a rolling "what did limits reject lately" view, not an
//! audit log — that honesty is acceptable for v1, and the `/v1/limits/status` docs say so.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::Serialize;

use lighttrack_core::{LimitMetric, LimitWindow};

/// How long a rejection key stays live after its last hit before it is pruned (rolling reset).
const TTL_HOURS: i64 = 24;

/// One `(project, metric, window)` rejection bucket.
struct Entry {
    count: u64,
    est_cost_usd: f64,
    first_ts: DateTime<Utc>,
    last_ts: DateTime<Utc>,
}

/// A read-only snapshot of one rejection bucket, shaped for the `/v1/limits/status` `rejected` block.
/// Timestamps are fixed-width `RFC3339(Nanos, Z)` for consistency with stored event times.
#[derive(Serialize, Clone, Debug)]
pub(crate) struct RejectionStat {
    pub(crate) metric: LimitMetric,
    pub(crate) window: LimitWindow,
    pub(crate) count: u64,
    pub(crate) est_missed_cost_usd: f64,
    pub(crate) first_ts: String,
    pub(crate) last_ts: String,
}

/// Thread-safe, self-pruning rejection ledger held in [`AppState`](crate::state::AppState).
pub(crate) struct RejectionLedger {
    inner: Mutex<HashMap<(String, LimitMetric, LimitWindow), Entry>>,
    ttl: Duration,
}

impl Default for RejectionLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl RejectionLedger {
    pub(crate) fn new() -> Self {
        Self { inner: Mutex::new(HashMap::new()), ttl: Duration::hours(TTL_HOURS) }
    }

    /// Record one rejected event against `(project, metric, window)` at `now`, folding in its
    /// estimated cost (`0.0` when the event was unpriced). Stale entries are pruned first. Returns the
    /// running rejection count for this key (including the one just recorded) so the caller can carry
    /// it into the breach alert. A poisoned lock degrades to a best-effort recovery (never blocks
    /// ingest, matching the rest of the in-memory alert state).
    pub(crate) fn record(
        &self,
        project: &str,
        metric: LimitMetric,
        window: LimitWindow,
        cost: f64,
        now: DateTime<Utc>,
    ) -> u64 {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        Self::prune(&mut map, now, self.ttl);
        let e = map.entry((project.to_string(), metric, window)).or_insert(Entry {
            count: 0,
            est_cost_usd: 0.0,
            first_ts: now,
            last_ts: now,
        });
        e.count += 1;
        e.est_cost_usd += cost;
        e.last_ts = now;
        e.count
    }

    /// Snapshot every live rejection bucket for `project` (pruning stale ones first), for the
    /// `/v1/limits/status` response. Order is unspecified.
    pub(crate) fn snapshot(&self, project: &str, now: DateTime<Utc>) -> Vec<RejectionStat> {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        Self::prune(&mut map, now, self.ttl);
        map.iter()
            .filter(|((p, _, _), _)| p == project)
            .map(|((_, metric, window), e)| RejectionStat {
                metric: *metric,
                window: *window,
                count: e.count,
                est_missed_cost_usd: e.est_cost_usd,
                first_ts: e.first_ts.to_rfc3339_opts(SecondsFormat::Nanos, true),
                last_ts: e.last_ts.to_rfc3339_opts(SecondsFormat::Nanos, true),
            })
            .collect()
    }

    /// Evict buckets whose last hit is older than the TTL (rolling reset).
    fn prune(
        map: &mut HashMap<(String, LimitMetric, LimitWindow), Entry>,
        now: DateTime<Utc>,
        ttl: Duration,
    ) {
        map.retain(|_, e| now.signed_duration_since(e.last_ts) < ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(base: DateTime<Utc>, secs: i64) -> DateTime<Utc> {
        base + Duration::seconds(secs)
    }

    #[test]
    fn record_increments_count_and_folds_cost() {
        let led = RejectionLedger::new();
        let now = Utc::now();
        assert_eq!(led.record("p", LimitMetric::CostUsd, LimitWindow::Day, 0.10, now), 1);
        assert_eq!(led.record("p", LimitMetric::CostUsd, LimitWindow::Day, 0.05, t(now, 1)), 2);
        let stats = led.snapshot("p", t(now, 2));
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].count, 2);
        assert!((stats[0].est_missed_cost_usd - 0.15).abs() < 1e-9);
    }

    #[test]
    fn keys_are_isolated_by_project_metric_and_window() {
        let led = RejectionLedger::new();
        let now = Utc::now();
        led.record("p", LimitMetric::CostUsd, LimitWindow::Day, 1.0, now);
        led.record("p", LimitMetric::Calls, LimitWindow::Day, 0.0, now); // same project, other metric
        led.record("p", LimitMetric::CostUsd, LimitWindow::Hour, 2.0, now); // other window
        led.record("q", LimitMetric::CostUsd, LimitWindow::Day, 9.0, now); // other project
        // p has three distinct buckets; q's is invisible to p.
        assert_eq!(led.snapshot("p", now).len(), 3);
        let q = led.snapshot("q", now);
        assert_eq!(q.len(), 1);
        assert!((q[0].est_missed_cost_usd - 9.0).abs() < 1e-9);
    }

    #[test]
    fn stale_entries_are_pruned() {
        let led = RejectionLedger::new();
        let base = Utc::now();
        led.record("p", LimitMetric::CostUsd, LimitWindow::Day, 1.0, base);
        // A hit 25h later prunes the (now-stale) original before recording the new one, so the count
        // resets rather than accumulating across the TTL boundary.
        let count = led.record("p", LimitMetric::CostUsd, LimitWindow::Day, 1.0, t(base, 25 * 3600));
        assert_eq!(count, 1, "stale bucket should have been pruned, restarting the count");
        // A snapshot far in the future prunes everything.
        assert!(led.snapshot("p", t(base, 60 * 3600)).is_empty());
    }
}
