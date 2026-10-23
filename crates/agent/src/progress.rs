//! Publish what this device is doing **while** it is doing it.
//!
//! The relay had a progress channel with no writer: `RelayTask.progress`, `POST
//! /v1/relay/tasks/:id/progress`, the fenced UPDATE in both store backends and the conformance
//! coverage all existed, and the word "progress" appeared in this crate exactly once — in a comment
//! explaining why the *renewal* is not progress-driven. So the one question the cloud could not
//! answer was the only one that matters between lease and settle: a task `leased` for ninety
//! minutes is either a long `edit` run doing its job or a device that is wedged, and the evidence
//! either way (which phase, how long, which workspace, how many renewals) sat in this process's
//! stack memory until the run ended. Kill every executor, open the view, and it said nothing.
//!
//! Three properties hold the design together.
//!
//! 1. **Progress is a read model, never a control channel.** `Emitter::emit` returns `()`: there is
//!    no error for the work to notice, and the publisher's answer is never classified into a
//!    [`crate::verdict::Verdict`]. Only the renewal decides ownership.
//! 2. **Publishing is off the work's thread.** Emission is a non-blocking channel send; the HTTP
//!    call happens on [`pump`]'s own thread. This is load-bearing, not tidiness: the renew cadence
//!    equals the lease TTL after the first renewal, so there is no slack to spend — a progress POST
//!    that sat on the thread that would otherwise renew could cost the device its lease, and a
//!    telemetry channel able to kill the work it reports on is worse than no channel.
//! 3. **The vocabulary is closed, and has nowhere to put content.** A phase names the action, the
//!    mode, the workspace's NAME, the model, the elapsed time, the renewal count and the first
//!    eight hex of the prompt fingerprint. Never prompt text, never result text — so an action with
//!    `report_io = false` publishes nothing new about what it said.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::cloud::Client;
use crate::connect::ConnectorSpec;

/// How often at most one line goes to the cloud.
///
/// Deliberately a constant rather than a fraction of the renewal cadence: coupling the two is how
/// liveness ends up waiting on the work having something to say.
pub(crate) const PUBLISH_WINDOW: Duration = Duration::from_secs(15);

/// How much of the prompt digest a liveness line carries — enough to join a run to a prompt
/// version, not enough to be a payload.
const FINGERPRINT_PREFIX: usize = 8;

/// The closed phase vocabulary. Every field here is metadata about the *shape* of the run; there is
/// deliberately no variant and no field that could carry the prompt or the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Phase {
    /// The action is being loaded and its prompt rendered. Nothing has been spent yet.
    Resolving {
        action: String,
        version: Option<String>,
    },
    /// The CLI is up. This is the span that can last hours and published nothing at all.
    Spawned {
        mode: &'static str,
        /// The workspace's declared NAME, never a resolved path.
        workspace: Option<String>,
        model: String,
        /// Full digest; only its prefix is ever rendered.
        fingerprint: String,
    },
    /// The CLI is down, with the envelope's subtype. Carries the connector kind so the delivering
    /// phase can name it without the publisher reaching into `deliver`.
    Exited {
        subtype: String,
        connector: Option<&'static str>,
    },
    /// The irreversible half is being attempted.
    Delivering,
    /// Reporting back to the cloud; after this the terminal record takes over.
    Settling,
}

/// The run-level facts a line is rendered against. A params struct because the renderer needs four
/// of them and a four-argument formatter is a four-argument bug.
pub(crate) struct Shown<'a> {
    pub elapsed: Duration,
    pub renewals: u32,
    pub fingerprint: Option<&'a str>,
    pub connector: Option<&'a str>,
}

/// Where a line goes. An abstraction with exactly one production implementation, so the publisher's
/// failure behaviour — the property that matters — is testable without a cloud.
pub(crate) trait Sink {
    fn publish(&self, line: &str) -> anyhow::Result<()>;
}

/// The production sink: one fenced POST per line, to the task's own progress door.
pub(crate) struct CloudSink<'a> {
    pub client: &'a Client,
    pub task_id: &'a str,
    pub fence: DateTime<Utc>,
}

impl Sink for CloudSink<'_> {
    fn publish(&self, line: &str) -> anyhow::Result<()> {
        self.client.progress(self.task_id, self.fence, line)
    }
}

/// The handle the work holds. `emit` cannot block, cannot fail and returns nothing — which is the
/// whole contract: a run must not be able to tell whether its telemetry landed.
pub(crate) struct Emitter {
    tx: Option<Sender<Phase>>,
}

impl Emitter {
    /// An emitter attached to nothing — for the unfenced legacy path (no fence, so no door to
    /// publish through) and for tests that are not about progress.
    pub(crate) fn detached() -> Self {
        Emitter { tx: None }
    }

    /// Hand a phase off. A full or dropped channel is not an error the run may learn about.
    pub(crate) fn emit(&self, phase: Phase) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(phase);
        }
    }
}

/// An emitter and the receiver [`pump`] consumes.
pub(crate) fn channel() -> (Emitter, Receiver<Phase>) {
    let (tx, rx) = std::sync::mpsc::channel();
    (Emitter { tx: Some(tx) }, rx)
}

/// At most one publish per window. The latest phase is what gets published when the window opens,
/// so a chatty phase costs one HTTP call and a phase *change* still lands inside one window rather
/// than waiting for a renewal interval.
pub(crate) struct Throttle {
    window: Duration,
    last: Option<Duration>,
}

impl Throttle {
    pub(crate) fn new(window: Duration) -> Self {
        Throttle { window, last: None }
    }

    /// May a line go out at `at` (time since the run started)? Records that it did.
    pub(crate) fn due(&mut self, at: Duration) -> bool {
        let open = self
            .last
            .is_none_or(|last| at.saturating_sub(last) >= self.window);
        if open {
            self.last = Some(at);
        }
        open
    }

    /// How long until the window reopens — what the pump waits on, so a phase change arriving mid
    /// window is published as soon as it may be and not one renewal later.
    fn wait(&self, at: Duration) -> Duration {
        match self.last {
            None => self.window,
            Some(last) => self.window.saturating_sub(at.saturating_sub(last)),
        }
    }
}

/// Drain phases, render them and publish them — on this thread and no other.
///
/// Runs until the emitter is dropped (the run is over). Every failure is logged and swallowed: the
/// caller has already decided what the run does, and it was not this.
pub(crate) fn pump<S: Sink>(
    sink: &S,
    rx: Receiver<Phase>,
    window: Duration,
    renewals: &AtomicU32,
    source: &str,
) {
    let start = Instant::now();
    let mut throttle = Throttle::new(window);
    let mut current: Option<Phase> = None;
    let mut fingerprint: Option<String> = None;
    let mut connector: Option<&'static str> = None;
    loop {
        match rx.recv_timeout(throttle.wait(start.elapsed())) {
            Ok(phase) => {
                match &phase {
                    Phase::Spawned { fingerprint: f, .. } => fingerprint = Some(f.clone()),
                    Phase::Exited { connector: c, .. } => connector = *c,
                    _ => {}
                }
                current = Some(phase);
            }
            // The window reopened: refresh the line in place, so "still running" carries a new
            // elapsed and renewal count rather than ageing silently.
            Err(RecvTimeoutError::Timeout) => {}
            // The run finished and dropped the emitter. Nothing is flushed here on purpose: a
            // final publish would put an HTTP call between the CLI exiting and the settle.
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let Some(phase) = &current else { continue };
        if !throttle.due(start.elapsed()) {
            continue;
        }
        let line = render(
            phase,
            &Shown {
                elapsed: start.elapsed(),
                renewals: renewals.load(Ordering::Relaxed),
                fingerprint: fingerprint.as_deref(),
                connector,
            },
        );
        // The person running the daemon sees exactly what the cloud sees: one line per task and
        // then silence was the local experience too.
        println!("[{source}] {line}");
        if let Err(e) = sink.publish(&line) {
            eprintln!("[{source}] progress not published (the run is unaffected): {e:#}");
        }
    }
}

/// One liveness line. Short, greppable, and structurally incapable of carrying content.
pub(crate) fn render(phase: &Phase, shown: &Shown<'_>) -> String {
    let head = match phase {
        Phase::Resolving { action, version } => match version {
            Some(v) => format!("resolving {action} {v}"),
            None => format!("resolving {action}"),
        },
        Phase::Spawned {
            mode,
            workspace,
            model,
            ..
        } => match workspace {
            Some(w) => format!("running claude {model} ({mode} in {w})"),
            None => format!("running claude {model} ({mode})"),
        },
        Phase::Exited { subtype, .. } => format!("claude exited ({subtype})"),
        Phase::Delivering => match shown.connector {
            Some(kind) => format!("delivering via {kind} connector"),
            None => "delivering the result".to_string(),
        },
        Phase::Settling => "settling".to_string(),
    };
    let mut line = format!(
        "{head} — {} elapsed, renewal {}",
        age(shown.elapsed),
        shown.renewals
    );
    if let Some(fp) = shown.fingerprint {
        let prefix: String = fp.chars().take(FINGERPRINT_PREFIX).collect();
        line.push_str(&format!(" [prompt {prefix}]"));
    }
    line
}

/// Which kind of connector an action declares, as a word fit for a liveness line.
///
/// It lives with the vocabulary rather than in `exec` on purpose: `exec` must not name `connect` at
/// all (a source guard in `deliver` pins that, and rightly — a delivery check that the next
/// refactor can move back inside the CLI span is no check), and naming a connector is not reaching
/// one.
pub(crate) fn connector_kind(spec: Option<&ConnectorSpec>) -> Option<&'static str> {
    match spec {
        Some(ConnectorSpec::Http { .. }) => Some("http"),
        Some(ConnectorSpec::Command { .. }) => Some("command"),
        None => None,
    }
}

fn age(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests;
