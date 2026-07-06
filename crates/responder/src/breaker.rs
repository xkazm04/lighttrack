//! Circuit breaker for the auto-fix (ACT) stage. Bounds how often the responder will modify a repo,
//! so a fix that itself errors can't drive an unbounded act → new-spike → act loop. Two limits: a
//! per-project cooldown and a global fixes-per-hour cap. `allow` only checks; `record` is called
//! after a fix is actually applied, so the hourly cap counts real edits, not skipped attempts.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const HOUR: Duration = Duration::from_secs(3600);

pub(crate) struct Breaker {
    last_act: Mutex<HashMap<String, Instant>>,
    recent: Mutex<Vec<Instant>>,
}

impl Breaker {
    pub(crate) fn new() -> Self {
        Breaker { last_act: Mutex::new(HashMap::new()), recent: Mutex::new(Vec::new()) }
    }

    /// `Ok(())` if an auto-fix is allowed for `project` right now, else `Err(reason)`.
    pub(crate) fn allow(
        &self,
        project: &str,
        cooldown: Duration,
        max_per_hour: u32,
    ) -> Result<(), String> {
        let now = Instant::now();
        let recent = self.recent.lock().unwrap();
        let count = recent.iter().filter(|t| now.duration_since(**t) < HOUR).count();
        if count as u32 >= max_per_hour {
            return Err(format!("hourly auto-fix cap reached ({max_per_hour}/h)"));
        }
        if let Some(t) = self.last_act.lock().unwrap().get(project) {
            if now.duration_since(*t) < cooldown {
                return Err(format!(
                    "'{project}' was auto-fixed within cooldown ({}s)",
                    cooldown.as_secs()
                ));
            }
        }
        Ok(())
    }

    /// Record that a fix was applied for `project` (updates cooldown + hourly counters).
    pub(crate) fn record(&self, project: &str) {
        let now = Instant::now();
        self.last_act.lock().unwrap().insert(project.to_string(), now);
        let mut recent = self.recent.lock().unwrap();
        recent.retain(|t| now.duration_since(*t) < HOUR);
        recent.push(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooldown_blocks_repeat() {
        let b = Breaker::new();
        assert!(b.allow("p", Duration::from_secs(3600), 5).is_ok());
        b.record("p");
        assert!(b.allow("p", Duration::from_secs(3600), 5).is_err()); // within cooldown
        assert!(b.allow("q", Duration::from_secs(3600), 5).is_ok()); // other project free
    }

    #[test]
    fn hourly_cap_blocks() {
        let b = Breaker::new();
        b.record("a");
        b.record("b");
        // Two fixes recorded; cap of 2 → the next project is blocked by the global hourly cap.
        assert!(b.allow("c", Duration::from_secs(1), 2).is_err());
        assert!(b.allow("c", Duration::from_secs(1), 3).is_ok());
    }
}
