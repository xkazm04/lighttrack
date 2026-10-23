//! Which seats are known-exhausted right now, and until when.
//!
//! This is what makes failover *seamless* rather than merely eventual: after the first call that
//! hits a usage limit, every following call for the next window goes straight to the fallback
//! instead of paying a failed attempt (and its latency) each time. Process-local and in memory on
//! purpose — a limit window is minutes to hours, a restart is rare, and the first call after one
//! simply rediscovers the state.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct Cooldowns {
    until: Mutex<HashMap<String, Instant>>,
}

impl Cooldowns {
    /// Mark `provider` exhausted for `for_how_long`. A longer existing hold is kept.
    pub fn mark(&self, provider: &str, for_how_long: Duration) {
        let until = Instant::now() + for_how_long;
        let mut map = self.until.lock().unwrap_or_else(|p| p.into_inner());
        let entry = map.entry(provider.to_string()).or_insert(until);
        if until > *entry {
            *entry = until;
        }
    }

    /// Seconds left on `provider`'s hold, or `None` when it may be tried.
    pub fn remaining(&self, provider: &str) -> Option<u64> {
        let mut map = self.until.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        match map.get(provider) {
            Some(until) if *until > now => Some((*until - now).as_secs().max(1)),
            Some(_) => {
                map.remove(provider);
                None
            }
            None => None,
        }
    }

    /// A successful call proves the seat is back; drop any hold early.
    pub fn clear(&self, provider: &str) {
        let mut map = self.until.lock().unwrap_or_else(|p| p.into_inner());
        map.remove(provider);
    }

    /// Every active hold, for the status surface.
    pub fn snapshot(&self) -> Vec<(String, u64)> {
        let map = self.until.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let mut out: Vec<(String, u64)> = map
            .iter()
            .filter(|(_, until)| **until > now)
            .map(|(p, until)| (p.clone(), (*until - now).as_secs().max(1)))
            .collect();
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marked_seat_is_skipped_until_its_hold_expires_or_is_cleared() {
        let c = Cooldowns::default();
        assert_eq!(c.remaining("anthropic"), None);
        c.mark("anthropic", Duration::from_secs(60));
        assert!(c.remaining("anthropic").unwrap() >= 59);
        assert_eq!(c.snapshot().len(), 1);
        c.clear("anthropic");
        assert_eq!(c.remaining("anthropic"), None);
    }

    #[test]
    fn a_shorter_mark_never_shortens_a_longer_hold() {
        let c = Cooldowns::default();
        c.mark("codex", Duration::from_secs(600));
        c.mark("codex", Duration::from_secs(5));
        assert!(c.remaining("codex").unwrap() > 500);
    }

    #[test]
    fn an_expired_hold_reads_as_none() {
        let c = Cooldowns::default();
        c.mark("codex", Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(c.remaining("codex"), None);
    }
}
