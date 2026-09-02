//! The two rolling detectors: an error spike, and a quality regression.
//!
//! Both stay **in process memory**, and that is a decision rather than a leftover. They are not
//! facts about the deployment — they are questions about *this replica's recent traffic* ("have I
//! seen five failures in five minutes"), answered from a bounded window. Persisting them would buy
//! nothing an operator can point at, and would put a store write on the ingest path for every
//! failed call. What *is* a fact — the alert that results — goes to the ledger like every other.

use std::collections::VecDeque;
use std::time::Instant;

use super::Alerter;

/// A detected burst of failures for one project — the payload of an error-spike alert.
#[derive(Clone)]
pub(crate) struct ErrorSpike {
    pub(crate) project_id: String,
    pub(crate) count: u32,
    pub(crate) window_secs: u64,
    pub(crate) model: String,
    pub(crate) status: String,
    pub(crate) error: Option<String>,
}

/// A detected quality regression: the recent mean score for one (project, rubric) has fallen well
/// below its baseline mean.
#[derive(Clone)]
pub(crate) struct ScoreDrop {
    pub(crate) project_id: String,
    pub(crate) rubric: String,
    pub(crate) recent_avg: f64,
    pub(crate) baseline_avg: f64,
    pub(crate) drop_pct: f64,
    pub(crate) samples: usize,
    pub(crate) scored_by: String,
}

impl Alerter {
    /// Push `now` into the project's rolling error window, evict entries older than the window, and
    /// return the current count. Takes an explicit `now` so it is unit-testable.
    pub(super) fn note_error(&self, project: &str, now: Instant) -> u32 {
        let mut map = self.error_windows.lock().unwrap_or_else(|p| p.into_inner());
        let dq = map.entry(project.to_string()).or_default();
        dq.push_back(now);
        while let Some(front) = dq.front() {
            if now.duration_since(*front) > self.config.error_window {
                dq.pop_front();
            } else {
                break;
            }
        }
        dq.len() as u32
    }

    /// Push a normalized score into the (project, rubric) window (capped at `score_window`) and,
    /// once there are enough samples, return `(recent_mean, baseline_mean, samples)` when the recent
    /// tail has regressed past the drop threshold. No I/O, so it is unit-testable.
    pub(super) fn note_score(&self, key: &str, normalized: f64) -> Option<(f64, f64, usize)> {
        let mut map = self.score_windows.lock().unwrap_or_else(|p| p.into_inner());
        let dq: &mut VecDeque<f64> = map.entry(key.to_string()).or_default();
        dq.push_back(normalized);
        while dq.len() > self.config.score_window {
            dq.pop_front();
        }
        let len = dq.len();
        if len < self.config.score_min_samples {
            return None;
        }
        let recent_k = (len / 4).max(3);
        let base_n = len.checked_sub(recent_k)?;
        if base_n < 3 {
            return None;
        }
        let recent: f64 = dq.iter().skip(base_n).sum::<f64>() / recent_k as f64;
        let baseline: f64 = dq.iter().take(base_n).sum::<f64>() / base_n as f64;
        if baseline <= 0.0 {
            return None;
        }
        if (baseline - recent) / baseline >= self.config.score_drop {
            Some((recent, baseline, len))
        } else {
            None
        }
    }
}
