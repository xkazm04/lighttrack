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
    run::run(&cfg, cli.once)
}
