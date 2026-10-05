//! The agent loop: round-robin over sources, lease → execute → settle, serially. Serial execution
//! is deliberate — one Claude Code run at a time respects the machine and the subscription window,
//! and the per-source rotation keeps one busy cloud from starving the others.
//!
//! Crash recovery is lease-based, not local: if the agent dies mid-run, the cloud reclaims the task
//! when its lease expires and the retry consumes an attempt — no local queue to reconcile. Since
//! M7 the lease is *renewable*, which is what lets that expiry be short (minutes of detection
//! latency) while a Claude Code run takes as long as it takes. A run whose renewal is refused stops
//! and does not deliver: its result would land on a task the cloud has already handed to somebody
//! else, and — unlike a stale write to a database row — the delivery half of an action is a
//! connector call the cloud cannot take back.
//!
//! Since M18 each lease also carries this device's **action inventory**, so the cloud hands it only
//! work it can actually run. The alternative was what shipped before: a device leased anything due,
//! discovered the action folder was missing, and burned a real attempt plus a five-hour retry
//! interval on that discovery — repeatedly, since nothing about the failure told the cloud to route
//! the task elsewhere.

use std::sync::mpsc::RecvTimeoutError;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;

use lighttrack_engine::{resolve_claude_bin, EngineConfig};

use crate::cloud::Client;
use crate::config::AgentConfig;
use crate::deliver;
use crate::exec;
use crate::report::RunReport;
use crate::verdict::Verdict;

pub(crate) fn run(cfg: &AgentConfig, once: bool) -> Result<()> {
    let engine = EngineConfig {
        claude_bin: resolve_claude_bin(&cfg.claude_bin),
        model: String::new(), // per-action models; the engine default is never used
        bare: false,          // subscription OAuth — the whole point of the relay
        // The relay runs actions, not rubrics; nothing here judges an `exec` dimension.
        sandbox: None,
    };
    let clients = cfg
        .sources
        .iter()
        .map(Client::new)
        .collect::<Result<Vec<_>>>()?;

    loop {
        // Re-enumerated each round rather than once at startup: an operator who adds an action
        // folder should not have to restart the agent for the cloud to start routing that work to
        // it. One directory walk per poll round is nothing beside a Claude Code run.
        let capabilities = crate::inventory::inventory(&cfg.actions_dir);
        let mut worked = false;
        for client in &clients {
            match client.lease(&capabilities, cfg.max_batch, cfg.lease_secs, cfg.wait_secs) {
                Ok(lease) => {
                    for task in lease.tasks {
                        worked = true;
                        println!(
                            "[{}] task {} ({}) attempt {}/{}",
                            client.name,
                            task.id,
                            task.action_type,
                            task.attempts,
                            task.max_attempts
                        );
                        run_one(client, cfg, &engine, &task, lease.renew_secs);
                    }
                }
                Err(e) => eprintln!("[{}] lease failed: {e:#}", client.name),
            }
        }
        if !worked {
            if once {
                return Ok(());
            }
            std::thread::sleep(Duration::from_secs(cfg.poll_secs));
        }
    }
}

/// Execute one leased task with a heartbeat alongside it, then settle it — fenced.
///
/// The heartbeat runs on a TIMER at the cadence the server handed back, never per unit of work: a
/// renewal loop driven by progress stops renewing inside the one step that takes an hour, which is
/// exactly the step during which the lease matters.
fn run_one(
    client: &Client,
    cfg: &AgentConfig,
    engine: &EngineConfig,
    task: &lighttrack_core::RelayTask,
    renew_secs: u64,
) {
    let Some(fence) = task.lease_fence else {
        // A cloud predating M7 stamps no fence. Run it the old way: unfenced, exactly as before.
        let report = deliver::propagate(exec::execute(cfg, engine, task));
        announce(client, task, &report);
        settle(client, task, None, &report);
        return;
    };

    let interrupt: Mutex<Option<Interrupt>> = Mutex::new(None);
    // The heartbeat waits on a channel, not a sleep: `thread::scope` joins the heartbeat before
    // the run's report is returned, and a sleeping heartbeat held every settle back for up to a
    // full renewal interval after the CLI had already exited — dead time on a serial loop, once
    // per task. Dropping the sender wakes it instantly.
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let interrupt_ref = &interrupt;
    let produced = std::thread::scope(|scope| {
        // `move` takes the receiver (not `Sync`) into the heartbeat; everything else it uses is a
        // shared reference or `Copy`.
        scope.spawn(move || {
            // Anything but a timeout means the run finished (sender dropped): nothing to keep alive.
            while let Err(RecvTimeoutError::Timeout) =
                done_rx.recv_timeout(Duration::from_secs(renew_secs))
            {
                let v = client.renew(&task.id, fence);
                let stop = match &v {
                    Verdict::Held { .. } => None,
                    // The operator's cancel, arriving on the only channel that reaches a running
                    // device. The lease is still ours, so this run still reports — it must only
                    // not deliver.
                    Verdict::Stop { .. } => Some(Interrupt::Cancelled),
                    // Affirmative evidence of a takeover. Nothing can interrupt the CLI mid-call,
                    // but neither the delivery nor the settle may happen on nobody's authority.
                    Verdict::Reclaimed { .. } | Verdict::NoSuchTask => Some(Interrupt::Lost),
                    // "I could not tell" is not evidence of a lost lease, and treating it as one
                    // would abandon a healthy run on a blip. That is what the TTL/3 cadence buys:
                    // room to miss one or two and try again.
                    Verdict::Unknown { .. } => None,
                };
                let Some(stop) = stop else {
                    if let Verdict::Unknown { err } = &v {
                        eprintln!("[{}] lease renewal failed (will retry): {err}", client.name);
                    }
                    continue;
                };
                eprintln!(
                    "[{}] {} on task {} — {}. {}",
                    client.name,
                    stop.headline(),
                    task.id,
                    v.describe(),
                    stop.consequence(),
                );
                *interrupt_ref.lock().unwrap_or_else(|e| e.into_inner()) = Some(stop);
                break;
            }
        });
        let produced = exec::execute(cfg, engine, task);
        drop(done_tx);
        produced
    });

    // THE seam: the result exists and nothing irreversible has happened yet.
    let interrupt = interrupt.into_inner().unwrap_or_else(|e| e.into_inner());
    let report = match settlement(interrupt.as_ref()) {
        Settlement::Abandon => return,
        Settlement::SettleOnly => produced.report,
        Settlement::DeliverAndSettle => deliver::propagate(produced),
    };
    announce(client, task, &report);
    settle(client, task, Some(fence), &report);
}

/// What the heartbeat learned while the CLI ran. The two interruptions are kept apart because their
/// settlements are opposite: a cancelled run is still ours to report, a reclaimed one is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interrupt {
    /// An operator asked this run to stop. The lease is still ours.
    Cancelled,
    /// Somebody else holds the task now, or it is gone.
    Lost,
}

impl Interrupt {
    /// Distinct wording on purpose: "you were cancelled" and "you were taken over" are different
    /// facts, and the device's log could say neither — because the agent could not tell them apart.
    fn headline(self) -> &'static str {
        match self {
            Interrupt::Cancelled => "CANCELLED by operator",
            Interrupt::Lost => "LEASE LOST",
        }
    }

    fn consequence(self) -> &'static str {
        match self {
            Interrupt::Cancelled => {
                "Its result will NOT be delivered; the run is still reported honestly."
            }
            Interrupt::Lost => "Its result will NOT be reported or delivered.",
        }
    }
}

/// What to do with a produced result. This decision is the point of the change: it sits between
/// producing the result and delivering it, which is the only place a cancel can still stop the
/// irreversible half. While the connector call lived inside `exec`, the seam did not exist and the
/// one stop flag was consulted after delivery had already happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Settlement {
    /// Nothing interrupted: propagate through the action's connector, then settle.
    DeliverAndSettle,
    /// Cancelled: skip the connector, and still settle — the store terminates a cancelling task as
    /// `cancelled` and charges it no failure, so reporting is the honest thing to do.
    SettleOnly,
    /// Lost: the result belongs to a run the cloud has handed away. Neither deliver nor settle.
    Abandon,
}

fn settlement(interrupt: Option<&Interrupt>) -> Settlement {
    match interrupt {
        None => Settlement::DeliverAndSettle,
        Some(Interrupt::Cancelled) => Settlement::SettleOnly,
        Some(Interrupt::Lost) => Settlement::Abandon,
    }
}

fn announce(client: &Client, task: &lighttrack_core::RelayTask, report: &RunReport) {
    match report.status {
        "succeeded" => println!("[{}] task {} succeeded", client.name, task.id),
        s => eprintln!(
            "[{}] task {} {}: {}",
            client.name,
            task.id,
            s,
            report.error.as_deref().unwrap_or("-")
        ),
    }
}

fn settle(
    client: &Client,
    task: &lighttrack_core::RelayTask,
    fence: Option<chrono::DateTime<chrono::Utc>>,
    report: &RunReport,
) {
    // The same classifier the renewal uses — one taxonomy, one authority. What was here was a
    // SECOND substring test over a formatted error string, free to drift from the first.
    match client.settle(&task.id, fence, report) {
        Verdict::Held { .. } | Verdict::Stop { .. } => {}
        // Not a failure to recover from: the cloud refused the report because this device no longer
        // owns the task, and whoever owns it now owns the outcome.
        Verdict::Reclaimed { status } => eprintln!(
            "[{}] settle {} REFUSED (409): this device no longer held the task{}; its result was \
             not recorded.",
            client.name,
            task.id,
            match status {
                Some(s) => format!(" (the record now holds '{s}')"),
                None => String::new(),
            }
        ),
        Verdict::NoSuchTask => eprintln!(
            "[{}] settle {} REFUSED (404): the task no longer exists; its result was not recorded.",
            client.name, task.id
        ),
        // Anything else self-heals — the lease expires and the cloud requeues.
        Verdict::Unknown { err } => {
            eprintln!("[{}] settle {} failed: {err}", client.name, task.id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decision this change creates. A cancelled run keeps its report and loses its delivery;
    /// a reclaimed run loses both; an uninterrupted run keeps both. Before the seam existed there
    /// was one boolean, consulted after `exec` had already delivered — so a cancelled run's
    /// connector had fired by the time anybody asked.
    #[test]
    fn the_stop_check_decides_between_producing_and_delivering() {
        assert_eq!(settlement(None), Settlement::DeliverAndSettle);
        assert_eq!(
            settlement(Some(&Interrupt::Cancelled)),
            Settlement::SettleOnly,
            "a cancelled run must not deliver, and must still report"
        );
        assert_eq!(
            settlement(Some(&Interrupt::Lost)),
            Settlement::Abandon,
            "a reclaimed run's result belongs to nobody"
        );
    }

    /// An operator reading a device log has to be able to tell the two apart. They were one message
    /// ("LEASE LOST") for both, because the agent could not tell them apart either.
    #[test]
    fn a_cancel_and_a_takeover_do_not_read_the_same() {
        assert_ne!(Interrupt::Cancelled.headline(), Interrupt::Lost.headline());
        assert!(Interrupt::Cancelled.headline().contains("CANCELLED"));
        assert!(Interrupt::Lost.headline().contains("LEASE LOST"));
        assert!(Interrupt::Cancelled
            .consequence()
            .contains("still reported"));
        assert!(Interrupt::Lost.consequence().contains("NOT be reported"));
    }
}
