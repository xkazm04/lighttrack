//! Circuit breaker + admission control for the responder's two paid stages.
//!
//! **ACT (auto-fix):** bounds how often the responder will modify a repo, so a fix that itself errors
//! can't drive an unbounded act → new-spike → act loop. Two limits: a per-project cooldown and a
//! global fixes-per-hour cap. `allow` only checks; `record` is called after a fix is actually applied.
//!
//! **INVESTIGATE:** the read stage runs first and always, and each run is a full (billable) Claude
//! Code child, so it — not ACT — is where a flapping project's spend actually accrues. Admission
//! control here has three layers: in-flight dedup (one run per project at a time), a per-project
//! cooldown, a rolling-hour spawn cap, and a global concurrency semaphore. Admission returns an RAII
//! [`InvestigationGuard`]; dropping it frees the concurrency permit and the in-flight slot.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const HOUR: Duration = Duration::from_secs(3600);

pub(crate) struct Breaker {
    last_act: Mutex<HashMap<String, Instant>>,
    recent: Mutex<Vec<Instant>>,
    /// Projects with an investigation currently in flight (in-flight dedup).
    inflight: Arc<Mutex<HashSet<String>>>,
    /// Last time each project was investigated (per-project cooldown).
    last_investigate: Mutex<HashMap<String, Instant>>,
    /// Investigation spawn timestamps over the last rolling hour (hourly spend cap).
    investigate_recent: Mutex<Vec<Instant>>,
    /// Global concurrency limit on in-flight investigations.
    investigate_sem: Arc<Semaphore>,
}

/// Held for the lifetime of one admitted investigation. On drop it releases the concurrency permit
/// and clears the project's in-flight slot, so the next spike for that project can be admitted.
pub(crate) struct InvestigationGuard {
    project: String,
    inflight: Arc<Mutex<HashSet<String>>>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for InvestigationGuard {
    fn drop(&mut self) {
        self.inflight.lock().unwrap().remove(&self.project);
    }
}

impl Breaker {
    pub(crate) fn new(max_concurrent_investigations: usize) -> Self {
        Breaker {
            last_act: Mutex::new(HashMap::new()),
            recent: Mutex::new(Vec::new()),
            inflight: Arc::new(Mutex::new(HashSet::new())),
            last_investigate: Mutex::new(HashMap::new()),
            investigate_recent: Mutex::new(Vec::new()),
            // At least one permit, so a misconfigured `0` doesn't wedge every investigation.
            investigate_sem: Arc::new(Semaphore::new(max_concurrent_investigations.max(1))),
        }
    }

    /// Decide whether an investigation for `project` may spawn *now*, and if so reserve the slot.
    ///
    /// Order: cheap read-only checks (cooldown, hourly cap) first, then an atomic in-flight reserve
    /// (check + insert under one lock, so a webhook retry or duplicate API instance can't both pass),
    /// then the concurrency permit. On success the cooldown/hourly counters are recorded and an RAII
    /// guard is returned; the caller holds it for the duration of the run.
    pub(crate) fn try_admit_investigation(
        &self,
        project: &str,
        cooldown: Duration,
        max_per_hour: u32,
    ) -> Result<InvestigationGuard, String> {
        let now = Instant::now();

        // Per-project cooldown — a flap fires the same spike repeatedly; this is what stops each one
        // buying a fresh paid run.
        if let Some(t) = self.last_investigate.lock().unwrap().get(project) {
            if now.duration_since(*t) < cooldown {
                return Err(format!(
                    "'{project}' was investigated within cooldown ({}s)",
                    cooldown.as_secs()
                ));
            }
        }

        // Global rolling-hour spawn cap.
        {
            let recent = self.investigate_recent.lock().unwrap();
            let count = recent.iter().filter(|t| now.duration_since(**t) < HOUR).count();
            if count as u32 >= max_per_hour {
                return Err(format!("hourly investigation cap reached ({max_per_hour}/h)"));
            }
        }

        // Atomic in-flight reserve: `insert` returns false if the project is already in flight.
        if !self.inflight.lock().unwrap().insert(project.to_string()) {
            return Err("an investigation for this project is already in flight".to_string());
        }

        // Concurrency permit. On failure, release the in-flight slot we just took.
        let permit = match self.investigate_sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                self.inflight.lock().unwrap().remove(project);
                return Err("max concurrent investigations reached".to_string());
            }
        };

        // Admitted — record cooldown + hourly counters.
        self.last_investigate.lock().unwrap().insert(project.to_string(), now);
        {
            let mut recent = self.investigate_recent.lock().unwrap();
            recent.retain(|t| now.duration_since(*t) < HOUR);
            recent.push(now);
        }

        Ok(InvestigationGuard {
            project: project.to_string(),
            inflight: self.inflight.clone(),
            _permit: permit,
        })
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
        let b = Breaker::new(2);
        assert!(b.allow("p", Duration::from_secs(3600), 5).is_ok());
        b.record("p");
        assert!(b.allow("p", Duration::from_secs(3600), 5).is_err()); // within cooldown
        assert!(b.allow("q", Duration::from_secs(3600), 5).is_ok()); // other project free
    }

    #[test]
    fn hourly_cap_blocks() {
        let b = Breaker::new(2);
        b.record("a");
        b.record("b");
        // Two fixes recorded; cap of 2 → the next project is blocked by the global hourly cap.
        assert!(b.allow("c", Duration::from_secs(1), 2).is_err());
        assert!(b.allow("c", Duration::from_secs(1), 3).is_ok());
    }

    #[test]
    fn investigation_dedup_and_cooldown() {
        let b = Breaker::new(4);
        let hour = Duration::from_secs(3600);

        // First admission succeeds.
        let g = b.try_admit_investigation("p", hour, 100).unwrap();
        // Second, while the first is in flight, is deduped.
        assert!(b.try_admit_investigation("p", hour, 100).is_err());
        // A different project is free.
        let _g2 = b.try_admit_investigation("q", hour, 100).unwrap();

        // Dropping the guard clears the in-flight slot, but the per-project cooldown still blocks a
        // fresh spawn for the same project.
        drop(g);
        assert!(b.try_admit_investigation("p", hour, 100).is_err()); // within cooldown
        // ...and a zero cooldown lets it back in once nothing is in flight.
        assert!(b.try_admit_investigation("p", Duration::from_secs(0), 100).is_ok());
    }

    #[test]
    fn investigation_concurrency_and_hourly_caps() {
        // Concurrency cap of 1: the second concurrent admission is shed.
        let b = Breaker::new(1);
        let zero = Duration::from_secs(0);
        let _g = b.try_admit_investigation("a", zero, 100).unwrap();
        assert!(b.try_admit_investigation("b", zero, 100).is_err()); // no permit free
        drop(_g);
        assert!(b.try_admit_investigation("b", zero, 100).is_ok()); // permit freed

        // Hourly cap: with a generous concurrency limit and zero cooldown, the rolling-hour spawn
        // count still bounds total runs.
        let b2 = Breaker::new(10);
        let g1 = b2.try_admit_investigation("x", zero, 2).unwrap();
        let g2 = b2.try_admit_investigation("y", zero, 2).unwrap();
        assert!(b2.try_admit_investigation("z", zero, 2).is_err()); // 2/h reached
        drop((g1, g2));
        assert!(b2.try_admit_investigation("z", zero, 2).is_err()); // still 2 in the last hour
    }
}
