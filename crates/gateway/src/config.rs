//! `gateway.toml`: which target serves a use case, and what stands in when its seat is exhausted.
//!
//! The file is committable — it names models and routes, never keys. Everything secret or
//! machine-specific (the API URL and key, the CLI binaries) comes from the environment.
//!
//! ```toml
//! cooldown_secs = 300                     # how long a seat that reported a usage limit is skipped
//!
//! [defaults]
//! fallback = ["codex/gpt-5.5@medium"]     # for a literal `provider/model` request with no route
//!
//! [routes.summarize-email]                # `model: "summarize-email"` in the request
//! primary  = "anthropic/claude-sonnet-5@medium"
//! fallback = ["codex/gpt-5.5@medium"]
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::target::Target;

/// Seconds a seat is skipped after it reported a usage limit and named no `retry_after`.
pub const DEFAULT_COOLDOWN_SECS: u64 = 300;

#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub routes: BTreeMap<String, Route>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        GatewayConfig {
            cooldown_secs: DEFAULT_COOLDOWN_SECS,
            defaults: Defaults::default(),
            routes: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Defaults {
    /// Targets tried, in order, when a literal request's seat fails over and its route has none.
    #[serde(default)]
    pub fallback: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Route {
    pub primary: String,
    #[serde(default)]
    pub fallback: Vec<String>,
}

fn default_cooldown() -> u64 {
    DEFAULT_COOLDOWN_SECS
}

impl GatewayConfig {
    /// Read and validate a config file. A missing file is an empty config — literal
    /// `provider/model` requests still work; only named routes need the file.
    pub fn load(path: &Path) -> Result<GatewayConfig> {
        if !path.exists() {
            return Ok(GatewayConfig::default());
        }
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: GatewayConfig =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Every target must parse, and a fallback that spends the same seat as the primary is a
    /// configuration error: it would be "tried" while the seat is still exhausted.
    pub fn validate(&self) -> Result<()> {
        for spec in &self.defaults.fallback {
            Target::parse(spec).map_err(|e| anyhow::anyhow!("[defaults].fallback: {e}"))?;
        }
        for (name, route) in &self.routes {
            let primary = Target::parse(&route.primary)
                .map_err(|e| anyhow::anyhow!("[routes.{name}].primary: {e}"))?;
            for spec in &route.fallback {
                let fb = Target::parse(spec)
                    .map_err(|e| anyhow::anyhow!("[routes.{name}].fallback: {e}"))?;
                if fb.same_seat(&primary) {
                    bail!(
                        "[routes.{name}]: fallback '{spec}' spends the same seat as primary '{}' \
                         — a usage limit on one is a usage limit on both, so it cannot stand in",
                        route.primary
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<GatewayConfig> {
        let cfg: GatewayConfig = toml::from_str(s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn a_route_with_a_cross_seat_fallback_is_accepted() {
        let cfg = parse(
            r#"
            [routes.summarize]
            primary = "anthropic/claude-sonnet-5@medium"
            fallback = ["codex/gpt-5.5@medium"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.cooldown_secs, DEFAULT_COOLDOWN_SECS);
        assert_eq!(cfg.routes["summarize"].fallback.len(), 1);
    }

    #[test]
    fn a_fallback_on_the_same_seat_is_a_config_error() {
        let err = parse(
            r#"
            [routes.summarize]
            primary = "anthropic/claude-sonnet-5"
            fallback = ["anthropic/claude-haiku-4-5"]
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("same seat"), "{err}");
    }

    #[test]
    fn a_missing_file_is_an_empty_config() {
        let cfg = GatewayConfig::load(Path::new("definitely/not/here.toml")).unwrap();
        assert!(cfg.routes.is_empty());
        assert_eq!(cfg.cooldown_secs, DEFAULT_COOLDOWN_SECS);
    }
}
