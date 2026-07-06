//! `lt-responder` — a local reactive loop for LightTrack.
//!
//! LightTrack POSTs an `error_spike` alert (via `LIGHTTRACK_ALERT_WEBHOOK`) to this service. For a
//! project we have mapped to a local repo, it classifies the failure (skipping transient/provider
//! errors), enriches it with the recent failing events pulled back from LightTrack, then runs
//! **Claude Code read-only** (`claude -p --permission-mode plan`) against the repo and writes a
//! diagnosis. Auto-fix is deliberately out of scope for this first cut.
//!
//! `main.rs` is wiring only — parse config, build the router, serve. All logic lives in the sibling
//! modules (config / webhook / classify / enrich / investigate / report / pipeline).

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

mod act;
mod breaker;
mod claude;
mod classify;
mod config;
mod enrich;
mod git;
mod investigate;
mod pipeline;
mod report;
mod state;
mod webhook;

use breaker::Breaker;
use config::Config;
use state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Arc::new(Config::from_env()?);
    let n_autofix = cfg.projects.values().filter(|p| p.auto_fix).count();
    println!(
        "lt-responder v{} on http://{}  (lighttrack={}, model={}, mode={}, budget=${:.2}, timeout={}s, projects={} ({} auto-fix), claude_bin={})",
        env!("CARGO_PKG_VERSION"),
        cfg.bind,
        cfg.lighttrack_url,
        cfg.defaults.model,
        cfg.defaults.permission_mode,
        cfg.defaults.max_budget_usd,
        cfg.defaults.timeout_secs,
        cfg.projects.len(),
        n_autofix,
        cfg.claude_bin,
    );
    if cfg.projects.is_empty() {
        eprintln!(
            "[responder] no projects mapped — set LIGHTTRACK_RESPONDER_MAP or create responder.map.json. \
             Spikes for unmapped projects are skipped."
        );
    }

    let state = AppState { cfg: cfg.clone(), breaker: Arc::new(Breaker::new()) };
    let app = Router::new()
        .route("/health", get(health))
        .route("/webhook", post(webhook::receive))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}
