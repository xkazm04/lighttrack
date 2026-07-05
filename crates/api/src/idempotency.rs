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

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

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
        Self { capacity: capacity.max(1), inner: Mutex::new(Inner::default()) }
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
}
