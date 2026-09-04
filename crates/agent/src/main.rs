//! lt-agent — the device side of the relay (docs/RELAY.md).
//!
//! Leases due tasks from one or more cloud LightTrack instances over outbound HTTPS, executes
//! each against the local (gitignored) action library with the Claude Code CLI, pushes results
//! into the originating apps via per-action connectors, and settles every task back to its cloud.
//!
//! This file is wiring only: parse args, load config, run the loop. Logic lives in the sibling
//! modules (`config`, `actions`, `inventory`, `exec`, `connect`, `cloud`, `report`, `run`).

mod actions;
mod cloud;
mod config;
mod connect;
mod exec;
mod inventory;
mod report;
mod run;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "lt-agent",
    about = "LightTrack device agent: run relay tasks with the local Claude Code CLI"
)]
struct Cli {
    /// Path to the agent config (TOML).
    #[arg(long, default_value = "agent.toml")]
    config: String,
    /// Drain every source once (keep leasing until a full round is empty), then exit.
    #[arg(long)]
    once: bool,
}

fn main() -> Result<()> {
    let _ = dotenvy::dotenv(); // device keys and connector secrets come from the environment
    let cli = Cli::parse();
    let cfg = config::AgentConfig::load(&cli.config)?;
    // The inventory is printed at startup because "why is nothing being picked up" should be
    // answerable here, not in the cloud's logs: since M18 a lease is filtered by what this device
    // advertises, so an empty or unexpected library is the first thing to check.
    // An agent with no runnable CLI would still lease tasks, fail each one, and burn a real attempt
    // (plus the retry interval) per task discovering it — the one failure the inventory filter
    // cannot catch, because the library is fine. Refuse to claim work instead, as the responder does.
    let probe = lighttrack_engine::probe(&lighttrack_engine::resolve_claude_bin(&cfg.claude_bin));
    println!("[lt-agent] {}", probe.summary());
    if !probe.installed {
        anyhow::bail!(
            "the Claude CLI is not runnable at '{}' — set claude_bin in {} or install it",
            cfg.claude_bin,
            cli.config
        );
    }
    let actions = inventory::inventory(&cfg.actions_dir);
    println!(
        "lt-agent v{}  device={} sources={} actions={} poll={}s
  advertising: {}",
        env!("CARGO_PKG_VERSION"),
        cfg.device,
        cfg.sources
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(","),
        cfg.actions_dir,
        cfg.poll_secs,
        inventory::describe(&actions),
    );
    // An empty inventory has two causes with opposite remedies, and the routing filter cannot tell
    // them apart by design (see inventory.rs). Say which one this is, so the banner's own question
    // — "why is nothing being picked up" — is answerable when the answer is a mistyped path.
    if actions.is_empty() {
        if let Some(why) = inventory::unreadable_reason(&cfg.actions_dir) {
            println!(
                "  NOTE: the action library at '{}' could not be read ({why}) — set actions_dir in {}. This device still leases every action type, because an unreadable library must not take it out of the fleet.",
                cfg.actions_dir, cli.config
            );
        }
    }
    run::run(&cfg, cli.once)
}
