//! `lt-gateway` — one OpenAI-compatible endpoint on localhost in front of the seat-metered CLIs.
//!
//! An app points its OpenAI SDK's `base_url` here and asks for a *use case* (`model:
//! "summarize-email"`) or a literal `provider/model[@effort]`. The gateway resolves the route
//! from `gateway.toml`, calls the engine's provider dispatch (`claude -p`, `codex exec`, or the
//! HTTP providers), fails over to the next seat when one is exhausted, and records every attempt
//! as an event in LightTrack.
//!
//! This file is wiring only. Handlers live in `chat` and `router`; the decisions in `routes`,
//! `failover`, `cooldown`, `admission` and `telemetry`.
//!
//! Env:
//!   LIGHTTRACK_GATEWAY_BIND     default 127.0.0.1:8790
//!   LIGHTTRACK_GATEWAY_CONFIG   default gateway.toml (missing = literal targets only)
//!   LIGHTTRACK_URL / LIGHTTRACK_KEY / LIGHTTRACK_PROJECT   where events go (unset = no telemetry)
//!   LIGHTTRACK_GATEWAY_OMIT_CONTENT=1   never send prompts/outputs on events
//!   LIGHTTRACK_GATEWAY_DEV=1    honour X-LightTrack-Simulate for failover drills
//!   LIGHTTRACK_CLAUDE_BIN / LIGHTTRACK_CODEX_BIN   CLI overrides (auto-resolved otherwise)

mod admission;
mod chat;
mod config;
mod cooldown;
mod error;
mod failover;
mod generator;
mod router;
mod routes;
mod state;
mod target;
mod telemetry;
mod wire;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let bind = std::env::var("LIGHTTRACK_GATEWAY_BIND").unwrap_or_else(|_| "127.0.0.1:8790".into());
    let config_path = PathBuf::from(
        std::env::var("LIGHTTRACK_GATEWAY_CONFIG").unwrap_or_else(|_| "gateway.toml".into()),
    );
    let cfg = config::GatewayConfig::load(&config_path)?;
    let telemetry = telemetry::Telemetry::from_env().map(Arc::new);
    let dev = matches!(
        std::env::var("LIGHTTRACK_GATEWAY_DEV").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    );

    eprintln!(
        "lt-gateway v{} on http://{bind}  routes={} ({})  telemetry={}  dev={dev}",
        env!("CARGO_PKG_VERSION"),
        cfg.routes.len(),
        config_path.display(),
        match &telemetry {
            Some(t) => t.url.clone(),
            None => "off (set LIGHTTRACK_KEY or LIGHTTRACK_PROJECT)".into(),
        },
    );
    for (name, r) in &cfg.routes {
        eprintln!("  {name}: {} -> {:?}", r.primary, r.fallback);
    }

    let state = state::AppState {
        cfg: Arc::new(cfg),
        gen: Arc::new(generator::EngineGenerator::from_env()),
        cooldowns: Arc::new(cooldown::Cooldowns::default()),
        telemetry,
        admission: Arc::new(admission::Admission::default()),
        dev,
    };
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    axum::serve(listener, router::build(state)).await?;
    Ok(())
}
