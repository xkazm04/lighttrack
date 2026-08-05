//! The per-source failure counter: how much guessing a source may do, and how little memory the
//! bookkeeping is allowed to cost.
//!
//! A fixed window rather than a sliding one, for a reason that is mostly about memory: the whole map
//! is dropped when the window rolls, so expiry costs nothing, needs no per-entry timestamps, and the
//! bound is exact. It also makes `Retry-After` *exact* — the time left in the window — instead of an
//! estimate a client has to guess at.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::ApiError;

/// Env: failed attempts one source may make per window before it is refused. `0` disables the
/// throttle entirely. Default [`DEFAULT_MAX_FAILURES`].
const ENV_MAX_FAILURES: &str = "LIGHTTRACK_AUTH_MAX_FAILURES";
/// Env: length of the failure window in seconds. Default [`DEFAULT_WINDOW_SECS`].
const ENV_WINDOW_SECS: &str = "LIGHTTRACK_AUTH_FAILURE_WINDOW_SECS";
/// Env: how many distinct sources may be tracked at once. Default [`DEFAULT_MAX_SOURCES`].
const ENV_MAX_SOURCES: &str = "LIGHTTRACK_AUTH_THROTTLE_MAX_SOURCES";
/// Env: number of **trusted** reverse proxies in front of this instance. `0` (the default) ignores
/// `X-Forwarded-For` completely. See [`super::source`].
const ENV_TRUSTED_HOPS: &str = "LIGHTTRACK_AUTH_TRUSTED_PROXY_HOPS";

/// Ten typos a minute is already an operator having a very bad time; a thousand is an attack. The
/// default has to be high enough that a fat-fingered key never locks anyone out, and low enough that
/// a weak admin key survives: 10/min is ~14k guesses a day, which no wordlist beats.
const DEFAULT_MAX_FAILURES: u32 = 10;
const DEFAULT_WINDOW_SECS: u64 = 60;
/// ~4k entries × (address string + counter) is well under a megabyte, and a deployment with more
/// than 4k *simultaneously failing* sources has a different problem than this one.
const DEFAULT_MAX_SOURCES: usize = 4096;

pub(crate) struct AuthThrottle {
    max_failures: u32,
    window: Duration,
    max_sources: usize,
    trusted_hops: usize,
    /// Tracked-source count mirrored outside the mutex, so the *success* path — which is the ingest
    /// hot path — skips the lock entirely while nobody on the instance is failing.
    tracked: AtomicUsize,
    state: Mutex<Failures>,
}

struct Failures {
    started: Instant,
    counts: HashMap<String, u32>,
    /// Entries replaced because the map was at capacity this window.
    evicted: u64,
}

impl AuthThrottle {
    pub(crate) fn from_env() -> Self {
        Self::new(
            env_parse(ENV_MAX_FAILURES).unwrap_or(DEFAULT_MAX_FAILURES),
            Duration::from_secs(env_parse(ENV_WINDOW_SECS).unwrap_or(DEFAULT_WINDOW_SECS)),
            env_parse(ENV_MAX_SOURCES).unwrap_or(DEFAULT_MAX_SOURCES),
            env_parse(ENV_TRUSTED_HOPS).unwrap_or(0),
        )
    }

    pub(crate) fn new(
        max_failures: u32,
        window: Duration,
        max_sources: usize,
        trusted_hops: usize,
    ) -> Self {
        Self {
            max_failures,
            // A zero window would roll on every attempt, i.e. silently disable the throttle while
            // reporting it as on. `LIGHTTRACK_AUTH_MAX_FAILURES=0` is the way to turn it off.
            window: window.max(Duration::from_millis(1)),
            max_sources: max_sources.max(1),
            trusted_hops,
            tracked: AtomicUsize::new(0),
            state: Mutex::new(Failures {
                started: Instant::now(),
                counts: HashMap::new(),
                evicted: 0,
            }),
        }
    }

    fn enabled(&self) -> bool {
        self.max_failures > 0
    }

    pub(crate) fn trusted_hops(&self) -> usize {
        self.trusted_hops
    }

    /// How many sources are currently tracked — the live memory bound.
    pub(crate) fn tracked(&self) -> usize {
        self.tracked.load(Ordering::Relaxed)
    }

    pub(crate) fn describe(&self) -> String {
        if !self.enabled() {
            return "off".to_string();
        }
        format!(
            "max_failures={}/{}s, max_sources={}, trusted_proxy_hops={}",
            self.max_failures,
            self.window_secs(),
            self.max_sources,
            self.trusted_hops
        )
    }

    fn window_secs(&self) -> u64 {
        self.window.as_secs().max(1)
    }

    /// A poisoned lock means some other thread panicked mid-update; the counters are still coherent
    /// enough to throttle with, and refusing to authenticate anyone would be the worse outcome.
    fn lock(&self) -> std::sync::MutexGuard<'_, Failures> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn roll(&self, f: &mut Failures) {
        if f.started.elapsed() >= self.window {
            f.counts.clear();
            f.evicted = 0;
            f.started = Instant::now();
            self.tracked.store(0, Ordering::Relaxed);
        }
    }

    /// `Err` once this source is out of budget. The `tracked() == 0` short-circuit is what keeps the
    /// check off the lock on an instance where nothing is failing — which is every healthy instance.
    pub(super) fn check(&self, source: &str) -> Result<(), ApiError> {
        if !self.enabled() || self.tracked() == 0 {
            return Ok(());
        }
        let mut f = self.lock();
        self.roll(&mut f);
        let count = f.counts.get(source).copied().unwrap_or(0);
        if count < self.max_failures {
            return Ok(());
        }
        // Exact for a fixed window, so the hint is a schedule rather than a guess. Rounded up so a
        // client that honours it to the second does not come back one tick early and burn a retry.
        let retry = self.window.saturating_sub(f.started.elapsed()).as_secs() + 1;
        Err(ApiError::rate_limited(format!(
            "too many failed authentication attempts from this address ({count} in the last {}s); \
             retry in {retry}s — this is a credential throttle, not a usage limit",
            self.window_secs()
        ))
        .retry_after(Some(retry)))
    }

    pub(super) fn success(&self, source: &str) {
        if !self.enabled() || self.tracked() == 0 {
            return;
        }
        let mut f = self.lock();
        if f.counts.remove(source).is_some() {
            self.tracked.store(f.counts.len(), Ordering::Relaxed);
        }
    }

    pub(super) fn failure(&self, source: &str) {
        if !self.enabled() {
            return;
        }
        let mut f = self.lock();
        self.roll(&mut f);
        if let Some(n) = f.counts.get_mut(source) {
            *n = n.saturating_add(1);
            if *n == self.max_failures {
                // Once per source per window: the operator's signal that someone is guessing.
                tracing::warn!(
                    source = %source,
                    failures = *n,
                    window_secs = self.window_secs(),
                    "throttling failed authentication attempts from this source"
                );
            }
            return;
        }
        if f.counts.len() >= self.max_sources {
            self.evict_one(&mut f);
        }
        f.counts.insert(source.to_string(), 1);
        self.tracked.store(f.counts.len(), Ordering::Relaxed);
    }

    /// Random replacement. The map is keyed by attacker-controlled input, so it must stay bounded —
    /// but going *blind* once full would let an attacker fill it from a botnet and then guess freely
    /// from one fresh address. Evicting an arbitrary entry only ever helps whoever is evicted, and an
    /// attacker with enough addresses to force evictions already had that many free budgets anyway.
    fn evict_one(&self, f: &mut Failures) {
        let Some(victim) = f.counts.keys().next().cloned() else {
            return;
        };
        f.counts.remove(&victim);
        f.evicted = f.evicted.saturating_add(1);
        if f.evicted == 1 {
            tracing::warn!(
                max_sources = self.max_sources,
                "failed-auth throttle is at capacity — an address-rotating attacker, or a very wide \
                 client fleet; entries are being replaced"
            );
        }
    }
}

fn env_parse<T: FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok().and_then(|s| s.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(max_failures: u32, max_sources: usize) -> AuthThrottle {
        AuthThrottle::new(max_failures, Duration::from_secs(60), max_sources, 0)
    }

    #[test]
    fn the_budget_is_spent_by_failures_and_restored_by_a_success() {
        let t = t(3, 16);
        for _ in 0..3 {
            assert!(t.check("1.2.3.4").is_ok());
            t.failure("1.2.3.4");
        }
        assert!(
            t.check("1.2.3.4").is_err(),
            "the 4th attempt must be refused"
        );
        // Another source is untouched: the counter is per-source, not global.
        assert!(t.check("5.6.7.8").is_ok());
        // A success wipes the record. (Reachable only below the threshold in real use — the guard
        // refuses before the credential is compared — which is exactly the point.)
        t.success("1.2.3.4");
        assert!(t.check("1.2.3.4").is_ok());
        assert_eq!(t.tracked(), 0);
    }

    #[test]
    fn a_disabled_throttle_never_refuses_and_never_allocates() {
        let t = t(0, 16);
        for _ in 0..1000 {
            t.failure("1.2.3.4");
        }
        assert!(t.check("1.2.3.4").is_ok());
        assert_eq!(t.tracked(), 0);
        assert_eq!(t.describe(), "off");
    }

    #[test]
    fn the_source_map_stays_bounded_under_address_rotation() {
        // The map is keyed by attacker-controlled input, so this bound *is* the DoS defence.
        let t = t(1, 8);
        for i in 0..5000u32 {
            t.failure(&format!("10.{}.{}.{}", i / 65536, (i / 256) % 256, i % 256));
        }
        assert_eq!(t.tracked(), 8, "the cap must hold no matter the input");
        // And we do not go blind once full: the newest offender is the one still being tracked.
        assert!(t.check("10.0.19.135").is_err());
    }

    #[test]
    fn the_window_rolls_and_frees_everything_it_held() {
        let t = AuthThrottle::new(1, Duration::from_millis(40), 16, 0);
        t.failure("1.2.3.4");
        assert!(t.check("1.2.3.4").is_err());
        std::thread::sleep(Duration::from_millis(60));
        assert!(t.check("1.2.3.4").is_ok(), "the window must decay");
        assert_eq!(t.tracked(), 0, "a rolled window drops the whole map");
    }

    #[test]
    fn retry_after_is_advertised_and_never_zero() {
        let t = AuthThrottle::new(1, Duration::from_secs(30), 16, 0);
        t.failure("1.2.3.4");
        let resp = axum::response::IntoResponse::into_response(t.check("1.2.3.4").unwrap_err());
        assert_eq!(resp.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        let secs: u64 = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
            .expect("a 429 without Retry-After gives a client nothing to honour");
        assert!((1..=31).contains(&secs), "retry-after was {secs}");
    }
}
