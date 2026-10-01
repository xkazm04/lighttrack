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
    /// The PRODUCER's verdict, carried through rather than re-derived downstream. `unknown` when the
    /// SDK did not send one — which is the signal the responder needs to know it must fall back to
    /// reading the message, instead of silently doing so for every event.
    pub(crate) failure_class: lighttrack_core::FailureClass,
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
    /// Push `now`, drop instants older than the window, and forget a project whose deque is then
    /// empty. The map is per replica; leaving quiet projects in it grows for the life of the
    /// process. Takes an explicit `now` so it is unit-testable.
    pub(super) fn note_error(&self, project: &str, now: Instant) -> u32 {
        let mut map = self.error_windows.lock().unwrap_or_else(|p| p.into_inner());
        let window = self.config.error_window;
        map.retain(|_, dq| {
            while let Some(front) = dq.front() {
                if now.duration_since(*front) > window {
                    dq.pop_front();
                } else {
                    break;
                }
            }
            !dq.is_empty()
        });
        let dq = map.entry(project.to_string()).or_default();
        dq.push_back(now);
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

/// The window the spike actually used. Whole minutes stay `5m`; every other length is seconds.
/// Integer minutes with a floor of one reported a 30s window as `1m`.
pub(super) fn window_phrase(secs: u64) -> String {
    if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// A window under 6 never has a tail of 3 and a baseline of 3. A minimum above the window never
/// fills. Both used to leave the startup banner claiming score-drop was on.
const SCORE_SAMPLE_FLOOR: usize = 4;
const SCORE_WINDOW_FLOOR: usize = 6;

pub(super) fn score_limits_from(window_raw: u64, min_raw: u64) -> (usize, usize) {
    let asked_window = (window_raw as usize).max(SCORE_SAMPLE_FLOOR);
    let asked_min = (min_raw as usize).max(SCORE_SAMPLE_FLOOR);
    let window = asked_window.max(SCORE_WINDOW_FLOOR);
    let min_samples = asked_min.min(window);
    if window != asked_window || min_samples != asked_min {
        tracing::warn!(
            window = window_raw,
            min_samples = min_raw,
            using_window = window,
            using_min = min_samples,
            "score-drop settings can never trip; using a window the detector can fire"
        );
    }
    (window, min_samples)
}

pub(super) fn score_limits() -> (usize, usize) {
    score_limits_from(
        super::env_u64("LIGHTTRACK_ALERT_SCORE_WINDOW", 20),
        super::env_u64("LIGHTTRACK_ALERT_SCORE_MIN_SAMPLES", 8),
    )
}

#[cfg(test)]
#[path = "detectors_tests.rs"]
mod tests;
