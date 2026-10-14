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
///
/// A key is **in flight** from [`begin`](Self::begin) until its [`Claim`] commits. A duplicate that
/// arrives in that window must NOT be acknowledged: the provider would mark the delivery done while
/// the first attempt could still roll back, and nothing would ever resend it. Only a key whose
/// writes committed reads as `Done`.
pub(crate) struct SeenWebhooks {
    capacity: usize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    set: HashSet<String>,
    in_flight: HashSet<String>,
    fifo: VecDeque<String>,
}

/// What `begin` found for a delivery key.
pub(crate) enum Begin {
    /// First sight: the caller owns the delivery and must `commit` the claim once it is durable.
    New(Claim),
    /// Already persisted: safe to acknowledge without re-running the writes.
    Done,
    /// Another attempt is still persisting it: do not acknowledge, the provider must retry.
    InFlight,
}

/// Ownership of one in-flight key. Dropping it uncommitted (store error, client disconnect, panic)
/// forgets the key, so the provider's retry is reprocessed instead of swallowed.
pub(crate) struct Claim {
    seen: std::sync::Arc<SeenWebhooks>,
    key: String,
    committed: bool,
}

impl Claim {
    /// The delivery is durably stored: from now on a repeat is `Done`.
    pub(crate) fn commit(mut self) {
        self.committed = true;
        let mut g = self.seen.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.in_flight.remove(&self.key);
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if !self.committed {
            self.seen.forget(&self.key);
        }
    }
}

impl SeenWebhooks {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Claim `key`. Evicts the oldest key when over capacity. A poisoned lock degrades to "not
    /// seen" (fail-open) so idempotency bookkeeping never blocks legitimate ingest.
    pub(crate) fn begin(self: &std::sync::Arc<Self>, key: &str) -> Begin {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if g.set.contains(key) {
            return if g.in_flight.contains(key) {
                Begin::InFlight
            } else {
                Begin::Done
            };
        }
        g.set.insert(key.to_string());
        g.in_flight.insert(key.to_string());
        g.fifo.push_back(key.to_string());
        while g.fifo.len() > self.capacity {
            if let Some(old) = g.fifo.pop_front() {
                g.set.remove(&old);
                g.in_flight.remove(&old);
            }
        }
        Begin::New(Claim {
            seen: self.clone(),
            key: key.to_string(),
            committed: false,
        })
    }

    /// Drop `key` from the seen-set so a later retry is reprocessed.
    fn forget(&self, key: &str) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.in_flight.remove(key);
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

    use std::sync::Arc;

    fn new_claim(seen: &Arc<SeenWebhooks>, k: &str) -> Claim {
        match seen.begin(k) {
            Begin::New(c) => c,
            _ => panic!("{k} should have been new"),
        }
    }

    #[test]
    fn first_sight_is_new_committed_repeat_is_done() {
        let seen = Arc::new(SeenWebhooks::new(16));
        new_claim(&seen, "wh_1").commit();
        assert!(matches!(seen.begin("wh_1"), Begin::Done));
        assert!(matches!(seen.begin("wh_2"), Begin::New(_))); // a different event is still new
    }

    /// The acknowledgement point: a duplicate that lands while the first attempt is still persisting
    /// must not be acked, because the first attempt may yet roll back.
    #[test]
    fn a_duplicate_while_in_flight_is_not_acknowledged() {
        let seen = Arc::new(SeenWebhooks::new(16));
        let claim = new_claim(&seen, "wh_1");
        assert!(matches!(seen.begin("wh_1"), Begin::InFlight));
        claim.commit();
        assert!(matches!(seen.begin("wh_1"), Begin::Done));
    }

    #[test]
    fn an_uncommitted_claim_is_forgotten_so_the_retry_is_reprocessed() {
        let seen = Arc::new(SeenWebhooks::new(16));
        drop(new_claim(&seen, "wh_1")); // store error, disconnect or panic
        new_claim(&seen, "wh_1").commit();
        assert!(matches!(seen.begin("wh_1"), Begin::Done));
    }

    #[test]
    fn evicts_oldest_over_capacity() {
        let seen = Arc::new(SeenWebhooks::new(2));
        new_claim(&seen, "a").commit();
        new_claim(&seen, "b").commit();
        new_claim(&seen, "c").commit(); // over cap -> evicts the oldest, "a"
        assert!(matches!(seen.begin("c"), Begin::Done));
        assert!(matches!(seen.begin("b"), Begin::Done));
        assert!(matches!(seen.begin("a"), Begin::New(_))); // evicted, reads as new again
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
