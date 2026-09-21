//! Walk a route's chain until one target answers.
//!
//! The question each failure asks is "would the *next seat* answer?", not "should we retry?" —
//! the engine already retried what was worth retrying. Three answers:
//!
//! - **Exhausted**: the seat itself is spent (a usage limit, a logged-out CLI). Try the next
//!   target *and* hold this seat out of the chain for a while, so the following calls skip
//!   straight to the fallback instead of paying a failed attempt each.
//! - **Transient**: this call failed for a reason another seat would not share (5xx, timeout, a
//!   crashed child). Try the next target; no hold.
//! - **Terminal**: the request itself is the problem (a rejected schema, an unparseable envelope).
//!   Another seat would fail the same way, and trying it would spend a second seat on one bad
//!   request. Stop and report.

use std::time::{Duration, Instant};

use serde_json::Value;

use lighttrack_engine::{ChatOutcome, ChatRequest, EngineError};

use crate::cooldown::Cooldowns;
use crate::generator::Generator;
use crate::target::Target;

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Exhausted(Duration),
    Transient,
    Terminal,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Exhausted(_) => "exhausted",
            Verdict::Transient => "transient",
            Verdict::Terminal => "terminal",
        }
    }
}

/// Phrases a CLI prints on stderr when the seat, not the call, is the problem. `claude -p` exits
/// non-zero with prose rather than a typed status, so this is the one place a string is read.
const SEAT_PHRASES: [&str; 6] = [
    "usage limit",
    "rate limit",
    "limit reached",
    "out of extra usage",
    "not logged in",
    "please run /login",
];

pub fn classify(err: &EngineError, default_hold: Duration) -> Verdict {
    match err {
        EngineError::RateLimited { retry_after, .. } => {
            Verdict::Exhausted(retry_after.unwrap_or(default_hold))
        }
        EngineError::OverBudgetWait { asked_secs, .. } => {
            Verdict::Exhausted(Duration::from_secs_f64(asked_secs.max(1.0)))
        }
        EngineError::Auth { .. } => Verdict::Exhausted(default_hold),
        EngineError::NonZero { stderr, .. } => {
            let lower = stderr.to_ascii_lowercase();
            if SEAT_PHRASES.iter().any(|p| lower.contains(p)) {
                Verdict::Exhausted(default_hold)
            } else {
                Verdict::Transient
            }
        }
        EngineError::ServerError { .. }
        | EngineError::Timeout { .. }
        | EngineError::Http { .. }
        | EngineError::EmptyCompletion { .. }
        | EngineError::Spawn { .. } => Verdict::Transient,
        EngineError::BadRequest { .. }
        | EngineError::Truncated { .. }
        | EngineError::Posture(_)
        | EngineError::Parse(_)
        | EngineError::Json(_)
        | EngineError::Other(_) => Verdict::Terminal,
    }
}

#[derive(Debug, Clone)]
pub enum AttemptKind {
    Served(ChatOutcome),
    Failed { error: String, verdict: Verdict },
    SkippedCooling { remaining_secs: u64 },
}

#[derive(Debug, Clone)]
pub struct Attempt {
    pub target: Target,
    pub latency_ms: u64,
    pub kind: AttemptKind,
}

/// Everything that happened for one request, in order. `served` indexes the attempt that
/// answered; `None` means the chain ran out.
#[derive(Debug, Clone)]
pub struct ChainResult {
    pub attempts: Vec<Attempt>,
    pub served: Option<usize>,
}

impl ChainResult {
    pub fn outcome(&self) -> Option<(&Target, &ChatOutcome)> {
        let a = &self.attempts[self.served?];
        match &a.kind {
            AttemptKind::Served(o) => Some((&a.target, o)),
            _ => None,
        }
    }

    /// True when the answer came from anything but the first target in the chain.
    pub fn fell_back(&self) -> bool {
        self.served.is_some_and(|i| i > 0)
    }
}

pub struct ChainRun<'a> {
    pub gen: &'a dyn Generator,
    pub cooldowns: &'a Cooldowns,
    pub default_hold: Duration,
    /// Dev-only: pretend this provider's seat is exhausted for this request.
    pub simulate_exhausted: Option<&'a str>,
}

impl ChainRun<'_> {
    pub fn run(&self, chain: &[Target], req: &ChatRequest) -> ChainResult {
        let mut attempts = Vec::with_capacity(chain.len());
        for target in chain {
            if let Some(remaining_secs) = self.cooldowns.remaining(&target.provider) {
                attempts.push(Attempt {
                    target: target.clone(),
                    latency_ms: 0,
                    kind: AttemptKind::SkippedCooling { remaining_secs },
                });
                continue;
            }
            let started = Instant::now();
            let result = if self.simulate_exhausted == Some(target.provider.as_str()) {
                Err(EngineError::RateLimited {
                    who: format!("{} (simulated)", target.provider),
                    retry_after: None,
                })
            } else {
                self.gen.generate(target, req)
            };
            let latency_ms = started.elapsed().as_millis() as u64;
            match result {
                Ok(outcome) => {
                    self.cooldowns.clear(&target.provider);
                    attempts.push(Attempt {
                        target: target.clone(),
                        latency_ms,
                        kind: AttemptKind::Served(outcome),
                    });
                    let served = Some(attempts.len() - 1);
                    return ChainResult { attempts, served };
                }
                Err(err) => {
                    let verdict = classify(&err, self.default_hold);
                    if let Verdict::Exhausted(hold) = verdict {
                        self.cooldowns.mark(&target.provider, hold);
                    }
                    let stop = verdict == Verdict::Terminal;
                    attempts.push(Attempt {
                        target: target.clone(),
                        latency_ms,
                        kind: AttemptKind::Failed {
                            error: err.to_string(),
                            verdict,
                        },
                    });
                    if stop {
                        break;
                    }
                }
            }
        }
        ChainResult {
            attempts,
            served: None,
        }
    }
}

/// The attempt list as a response or an event carries it.
pub fn report(result: &ChainResult) -> Vec<Value> {
    result
        .attempts
        .iter()
        .map(|a| {
            let (outcome, error) = match &a.kind {
                AttemptKind::Served(_) => ("served", None),
                AttemptKind::Failed { error, verdict } => (verdict.as_str(), Some(error.clone())),
                AttemptKind::SkippedCooling { remaining_secs } => (
                    "skipped_cooling",
                    Some(format!("seat on hold for {remaining_secs}s more")),
                ),
            };
            let mut v = serde_json::json!({
                "target": a.target.spec(),
                "outcome": outcome,
                "latency_ms": a.latency_ms,
            });
            if let Some(e) = error {
                v["error"] = Value::String(e);
            }
            v
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests;
