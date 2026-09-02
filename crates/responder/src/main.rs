//! `lt-responder` — a local reactive loop for LightTrack.
//!
//! LightTrack POSTs an `error_spike` alert (via `LIGHTTRACK_ALERT_WEBHOOK`) to this service. For a
//! project we have mapped to a local repo, it classifies the failure (skipping transient/provider
//! errors), enriches it with the recent failing events pulled back from LightTrack, then runs
//! **Claude Code read-only** (`claude -p --permission-mode plan`) against the repo and writes a
//! diagnosis, optionally applying a gated auto-fix on a review branch. Every Claude run goes
//! through the engine's one invocation seam (`lighttrack_engine::invocation`), which decides the
//! posture: the investigation is a read-only scan, the fix is an edit run.
//!
//! `main.rs` is wiring only — parse config, build the router, serve. All logic lives in the sibling
//! modules (config / webhook / classify / enrich / investigate / report / pipeline).

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

mod act;
mod breaker;
mod classify;
mod config;
mod email;
mod enrich;
mod git;
mod investigate;
mod invoke;
mod ledger;
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
    let email = match &cfg.email {
        Some(e) => format!("on({})", e.recipients()),
        None => "off".to_string(),
    };
    println!(
        "lt-responder v{} on http://{}  (lighttrack={}, model={}, mode={}, budget=${:.2}, timeout={}s, projects={} ({} auto-fix), email={email}, claude_bin={})",
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
    // A responder with no usable CLI would still accept webhooks, spend an investigation slot on
    // each, and file a diagnosis that is only an error message. Refuse to claim the work instead.
    let probe = lighttrack_engine::probe(&cfg.claude_bin);
    println!("[responder] {}", probe.summary());
    if !probe.installed {
        anyhow::bail!(
            "the Claude CLI is not runnable at '{}' — set LIGHTTRACK_RESPONDER_CLAUDE_BIN or install it",
            cfg.claude_bin
        );
    }

    if cfg.projects.is_empty() {
        eprintln!(
            "[responder] no projects mapped — set LIGHTTRACK_RESPONDER_MAP or create responder.map.json. \
             Spikes for unmapped projects are skipped."
        );
    }

    let state = AppState {
        cfg: cfg.clone(),
        breaker: Arc::new(Breaker::new(cfg.defaults.max_concurrent_investigations)),
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/webhook", post(webhook::receive))
        .with_state(state);

    // An unsigned /webhook on a non-loopback bind is an unauthenticated way to spend money and
    // edit a repo. Say so once, loudly, rather than letting it be a quiet default.
    if cfg.webhook_secret.is_none() && !config::bind_is_loopback(&cfg.bind) {
        eprintln!(
            "[responder] WARNING: bound to {} with no LIGHTTRACK_RESPONDER_WEBHOOK_SECRET — \
             anyone who can reach this port can spend a Claude run and trigger an auto-fix",
            cfg.bind
        );
    }
    if cfg.api_key.is_none() {
        eprintln!(
            "[responder] no LIGHTTRACK_API_KEY set — context enrichment will read unauthenticated \
             (fine in dev mode, empty against an enforcing deployment) and diagnoses will not be \
             posted back as alert resolutions"
        );
    }

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}
