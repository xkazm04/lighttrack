//! lt-agent — the device side of the relay (docs/RELAY.md).
//!
//! Leases due tasks from one or more cloud LightTrack instances over outbound HTTPS, executes
//! each against the local (gitignored) action library with the Claude Code CLI, pushes results
//! into the originating apps via per-action connectors, and settles every task back to its cloud.
//!
//! This file is wiring only: parse args, load config, run the loop. Logic lives in the sibling
//! modules (`config`, `actions`, `exec`, `connect`, `cloud`, `run`).

mod actions;
mod cloud;
mod config;
mod connect;
mod exec;
mod run;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "lt-agent", about = "LightTrack device agent: run relay tasks with the local Claude Code CLI")]
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
    println!(
        "lt-agent v{}  device={} sources={} actions={} poll={}s",
        env!("CARGO_PKG_VERSION"),
        cfg.device,
        cfg.sources.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(","),
        cfg.actions_dir,
        cfg.poll_secs,
    );
    run::run(&cfg, cli.once)
}
