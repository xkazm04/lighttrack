//! The agent loop: round-robin over sources, lease → execute → settle, serially. Serial execution
//! is deliberate — one Claude Code run at a time respects the machine and the subscription window,
//! and the per-source rotation keeps one busy cloud from starving the others.
//!
//! Crash recovery is lease-based, not local: if the agent dies mid-run, the cloud reclaims the
//! task when its lease expires and the retry consumes an attempt — no local queue to reconcile.

use anyhow::Result;

use lighttrack_engine::{resolve_claude_bin, EngineConfig};

use crate::cloud::Client;
use crate::config::AgentConfig;
use crate::exec;

pub(crate) fn run(cfg: &AgentConfig, once: bool) -> Result<()> {
    let engine = EngineConfig {
        claude_bin: resolve_claude_bin(&cfg.claude_bin),
        model: String::new(), // per-action models; the engine default is never used
        bare: false,          // subscription OAuth — the whole point of the relay
    };
    let clients = cfg.sources.iter().map(Client::new).collect::<Result<Vec<_>>>()?;

    loop {
        let mut worked = false;
        for client in &clients {
            match client.lease(&cfg.device, cfg.max_batch, cfg.lease_secs, cfg.wait_secs) {
                Ok(tasks) => {
                    for task in tasks {
                        worked = true;
                        println!(
                            "[{}] task {} ({}) attempt {}/{}",
                            client.name, task.id, task.action_type, task.attempts, task.max_attempts
                        );
                        let report = exec::execute(cfg, &engine, &task);
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
                        if let Err(e) = client.settle(&task.id, &report) {
                            // Settle failures self-heal: the lease expires and the cloud requeues.
                            eprintln!("[{}] settle {} failed: {e:#}", client.name, task.id);
                        }
                    }
                }
                Err(e) => eprintln!("[{}] lease failed: {e:#}", client.name),
            }
        }
        if !worked {
            if once {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_secs(cfg.poll_secs));
        }
    }
}
