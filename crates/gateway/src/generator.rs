//! The seam between the gateway and the engine — one trait, so the failover logic and the HTTP
//! surface are testable without a CLI on the machine.

use serde_json::Value;

use lighttrack_engine::{generate, EngineConfig, GenOutcome, Result};

use crate::target::Target;

pub trait Generator: Send + Sync {
    fn generate(
        &self,
        target: &Target,
        system: Option<&str>,
        input: &str,
        schema: Option<&Value>,
    ) -> Result<GenOutcome>;
}

/// The real thing: the engine's provider dispatch, which already knows `claude -p`, `codex exec`
/// and the HTTP providers, with retry, schema fallback and bounded timeouts.
pub struct EngineGenerator {
    pub cfg: EngineConfig,
}

impl EngineGenerator {
    pub fn from_env() -> EngineGenerator {
        let given = std::env::var("LIGHTTRACK_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
        let cfg = EngineConfig {
            claude_bin: lighttrack_engine::resolve_claude_bin(&given),
            ..EngineConfig::default()
        };
        EngineGenerator { cfg }
    }
}

impl Generator for EngineGenerator {
    fn generate(
        &self,
        target: &Target,
        system: Option<&str>,
        input: &str,
        schema: Option<&Value>,
    ) -> Result<GenOutcome> {
        generate(
            &self.cfg,
            &target.provider,
            &target.model,
            system,
            input,
            schema,
        )
    }
}
