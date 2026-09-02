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

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;

use lighttrack_engine::{resolve_claude_bin, EngineConfig};

use crate::cloud::Client;
use crate::config::AgentConfig;
use crate::exec::{self, RunReport};

pub(crate) fn run(cfg: &AgentConfig, once: bool) -> Result<()> {
    let engine = EngineConfig {
        claude_bin: resolve_claude_bin(&cfg.claude_bin),
        model: String::new(), // per-action models; the engine default is never used
        bare: false,          // subscription OAuth — the whole point of the relay
    };
    let clients = cfg
        .sources
        .iter()
        .map(Client::new)
        .collect::<Result<Vec<_>>>()?;

    loop {
        let mut worked = false;
        for client in &clients {
            match client.lease(&cfg.device, cfg.max_batch, cfg.lease_secs, cfg.wait_secs) {
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
        let report = exec::execute(cfg, engine, task);
        announce(client, task, &report);
        settle(client, task, None, &report);
        return;
    };

    let finished = AtomicBool::new(false);
    let lost = AtomicBool::new(false);
    let report = std::thread::scope(|scope| {
        scope.spawn(|| {
            while !finished.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(renew_secs));
                if finished.load(Ordering::Relaxed) {
                    break;
                }
                match client.renew(&task.id, fence) {
                    Ok(true) => {}
                    Ok(false) => {
                        // Affirmative evidence of a takeover (or a cancellation). Nothing can
                        // interrupt the CLI mid-call, but the delivery this run would have made is
                        // suppressed — the settle below is skipped entirely.
                        eprintln!(
                            "[{}] LEASE LOST on task {} — this run is no longer ours. Its result \
                             will NOT be reported or delivered.",
                            client.name, task.id
                        );
                        lost.store(true, Ordering::Relaxed);
                        break;
                    }
                    // A transient failure is not evidence of a lost lease, and treating it as one
                    // would abandon a healthy run on a blip. That is what the TTL/3 cadence buys:
                    // room to miss one or two and try again.
                    Err(e) => {
                        eprintln!("[{}] lease renewal failed (will retry): {e:#}", client.name)
                    }
                }
            }
        });
        let report = exec::execute(cfg, engine, task);
        finished.store(true, Ordering::Relaxed);
        report
    });

    if lost.load(Ordering::Relaxed) {
        return;
    }
    announce(client, task, &report);
    settle(client, task, Some(fence), &report);
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
    if let Err(e) = client.settle(&task.id, fence, report) {
        // A 409 is not a failure to recover from: the cloud refused the report because this device
        // no longer owns the task, and whoever owns it now owns the outcome. Anything else
        // self-heals — the lease expires and the cloud requeues.
        if crate::cloud::is_conflict(&e) {
            eprintln!(
                "[{}] settle {} REFUSED (409): this device no longer held the task; its result \
                 was not recorded.",
                client.name, task.id
            );
        } else {
            eprintln!("[{}] settle {} failed: {e:#}", client.name, task.id);
        }
    }
}
