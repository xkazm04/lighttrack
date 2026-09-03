//! Webhook-delivery idempotency: collapse duplicate / replayed provider deliveries at ingest.
//!
//! Providers retry a webhook on any non-2xx, and Polar (Standard Webhooks) may deliver a single
//! event more than once — each delivery carrying a stable `webhook-id`. We remember the recently-seen
//! delivery ids and short-circuit reprocessing of one we've already handled.
//!
//! This is the *cheap, in-process* layer. The durable guarantee is the deterministic
//! `revenue_events.id` upsert (see `lighttrack_billing::polar`): even a cache miss — process restart,
//! eviction — reprocesses to the same rows, so a missed dedup never double-counts. The two layers
//! compose: this one saves redundant store writes and gives explicit per-event idempotency; the
//! upsert is the backstop. Note this dedups *redelivery of the same event*; two **different** Polar
//! events for one refund collapse via the canonical record key, not here.
//!
//! The same bookkeeping, for a different redundant write: [`touch_key_later`] debounces the
//! per-request `last_used_at` stamp on an API key.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::Utc;

use crate::state::AppState;

/// Default capacity — covers a realistic retry-storm window without unbounded growth.
pub(crate) const DEFAULT_CAPACITY: usize = 8192;

/// A bounded, thread-safe set of recently-seen idempotency keys with FIFO eviction.
pub(crate) struct SeenWebhooks {
    capacity: usize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    set: HashSet<String>,
    fifo: VecDeque<String>,
}

impl SeenWebhooks {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Record `key` as seen and report whether it was **already** present (a duplicate delivery).
    /// Evicts the oldest key when over capacity. A poisoned lock degrades to "not seen" (fail-open)
    /// so idempotency bookkeeping never blocks legitimate ingest.
    pub(crate) fn check_and_insert(&self, key: &str) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if g.set.contains(key) {
            return true;
        }
        g.set.insert(key.to_string());
        g.fifo.push_back(key.to_string());
        while g.fifo.len() > self.capacity {
            if let Some(old) = g.fifo.pop_front() {
                g.set.remove(&old);
            }
        }
        false
    }

    /// Drop `key` from the seen-set so a later retry is reprocessed — call this when processing the
    /// delivery failed, so a transient error doesn't permanently swallow the provider's retries.
    pub(crate) fn forget(&self, key: &str) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if g.set.remove(key) {
            g.fifo.retain(|k| k != key);
        }
    }
}

/// Under this interval a key's `last_used_at` is not rewritten again: the value is "used recently",
/// and one write per key per minute says that as well as one per request.
const KEY_TOUCH_INTERVAL: Duration = Duration::from_secs(60);
/// Bound on the debounce map; past it, stale entries are swept before a new key is admitted.
const KEY_TOUCH_MAX_TRACKED: usize = 4096;

/// Record that `key_id` was just used — best-effort and detached, so it never delays the request.
///
/// Debounced: this used to fire a store write on EVERY authenticated request, which on SQLite put
/// one write transaction per call in the same single-writer queue as ingest itself, and under a
/// burst competed with the very events it was authenticating. `last_used_at` still moves — at most
/// once per key per [`KEY_TOUCH_INTERVAL`], which is the granularity anyone reads it at.
pub(crate) fn touch_key_later(st: &AppState, key_id: &str) {
    static LAST: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    let due = {
        let mut m = LAST
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        touch_due(&mut m, key_id, Instant::now(), KEY_TOUCH_INTERVAL)
    };
    if !due {
        return;
    }
    let store = st.store.clone();
    let id = key_id.to_string();
    tokio::spawn(async move {
        let r = tokio::task::spawn_blocking(move || store.touch_api_key(&id, Utc::now())).await;
        if let Ok(Err(e)) = r {
            tracing::warn!(error = %e, "could not record an API key's last use");
        }
    });
}

/// Stamp `now` for `id` and say whether a write is due. Pure, so the debounce is testable.
fn touch_due(m: &mut HashMap<String, Instant>, id: &str, now: Instant, every: Duration) -> bool {
    if let Some(t) = m.get(id) {
        if now.duration_since(*t) < every {
            return false;
        }
    } else if m.len() >= KEY_TOUCH_MAX_TRACKED {
        m.retain(|_, t| now.duration_since(*t) < every);
    }
    m.insert(id.to_string(), now);
    true
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sight_is_new_repeat_is_duplicate() {
        let seen = SeenWebhooks::new(16);
        assert!(!seen.check_and_insert("wh_1")); // first delivery
        assert!(seen.check_and_insert("wh_1")); // retry → duplicate
        assert!(!seen.check_and_insert("wh_2")); // a different event is still new
    }

    #[test]
    fn forget_allows_reprocessing() {
        let seen = SeenWebhooks::new(16);
        assert!(!seen.check_and_insert("wh_1"));
        seen.forget("wh_1");
        // After a failed-delivery forget, the provider's retry is reprocessed, not swallowed.
        assert!(!seen.check_and_insert("wh_1"));
        assert!(seen.check_and_insert("wh_1"));
    }

    #[test]
    fn evicts_oldest_over_capacity() {
        let seen = SeenWebhooks::new(2);
        assert!(!seen.check_and_insert("a"));
        assert!(!seen.check_and_insert("b"));
        assert!(!seen.check_and_insert("c")); // over cap → evicts the oldest, "a"
                                              // "b" and "c" are still within the window (a re-check doesn't disturb the FIFO order).
        assert!(seen.check_and_insert("c"));
        assert!(seen.check_and_insert("b"));
        // "a" was evicted, so it reads as new again.
        assert!(!seen.check_and_insert("a"));
    }

    /// One write per key per interval, however many requests arrive; a stale entry is refreshed;
    /// and when the map is full, only stale entries are swept to make room.
    #[test]
    fn a_key_is_touched_once_per_interval_and_the_map_stays_bounded() {
        let every = Duration::from_secs(60);
        let t0 = Instant::now();
        let mut m = HashMap::new();
        assert!(touch_due(&mut m, "k", t0, every));
        assert!(!touch_due(&mut m, "k", t0 + Duration::from_secs(30), every));
        assert!(touch_due(&mut m, "k", t0 + Duration::from_secs(61), every));

        let mut full: HashMap<String, Instant> = (0..KEY_TOUCH_MAX_TRACKED)
            .map(|i| (format!("old-{i}"), t0))
            .collect();
        full.insert("fresh".into(), t0 + Duration::from_secs(100));
        assert!(touch_due(
            &mut full,
            "new",
            t0 + Duration::from_secs(120),
            every
        ));
        assert!(full.contains_key("fresh") && full.contains_key("new"));
        assert_eq!(
            full.len(),
            2,
            "the stale entries were swept, the fresh one kept"
        );
    }
}
