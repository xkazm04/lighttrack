//! Agent configuration (`agent.toml`). Device keys are named by env var, never inlined — the
//! config file is committable; secrets stay in the environment / `.env`.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(crate) struct AgentConfig {
    /// Local label for this device, printed in the startup banner.
    ///
    /// **No longer the cloud's idea of who this is** (M18): identity comes from the device key, so
    /// the cloud resolves it against its own `devices` table and stamps that id on a leased task.
    /// The field stays because an operator running several agents wants their logs told apart, and
    /// because removing it would fail every existing `agent.toml` at parse time.
    #[serde(default = "default_device")]
    pub device: String,
    /// Seconds to sleep after a round of empty polls.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
    /// Long-poll: ask each source to hold the lease request up to this many seconds (server caps
    /// at 25). 0 = plain polling. With several sources keep it small — a held poll on one source
    /// delays the round-robin for the others.
    #[serde(default)]
    pub wait_secs: u64,
    /// How long a leased task may go without a heartbeat before the cloud may reclaim it. Since M7
    /// the agent renews the lease on a timer the server hands back (about a third of this), so this
    /// is **detection latency for a dead agent**, not a bound on how long a Claude run may take —
    /// a run takes as long as it takes. The server clamps what it will grant.
    #[serde(default = "default_lease_secs")]
    pub lease_secs: i64,
    /// Tasks leased per call. Runs are serial, so a small batch just saves round trips.
    #[serde(default = "default_max_batch")]
    pub max_batch: usize,
    /// Root of the local action library (see `actions/README.md`).
    #[serde(default = "default_actions_dir")]
    pub actions_dir: String,
    /// Root under which an action's `workspace` is resolved. Unset means this device runs no
    /// scan/edit actions at all — the opt-in is the operator's, and it is a directory they name,
    /// so a repository can only be reached if they pointed at its parent.
    #[serde(default)]
    pub workspaces_root: Option<PathBuf>,
    /// Claude executable; the default auto-resolves the npm `claude.exe` on Windows.
    #[serde(default = "default_claude_bin")]
    pub claude_bin: String,
    pub sources: Vec<Source>,
}

/// One cloud LightTrack instance to lease from.
#[derive(Debug, Deserialize)]
pub(crate) struct Source {
    pub name: String,
    pub url: String,
    /// Env var holding this source's **device key** — an enrolled `ltd_…` key from
    /// `POST /v1/relay/devices` (M18), or the deprecated shared `LIGHTTRACK_RELAY_DEVICE_KEY` for
    /// one more release. Named, never inlined: `agent.toml` is committable and this is a secret.
    ///
    /// Per source rather than global because one device can serve several clouds, and each issues
    /// its own key — which is the whole point of per-device keys: revoking one cloud's access to
    /// this machine must not touch another's.
    pub device_key_env: String,
}

impl Source {
    pub(crate) fn key(&self) -> Result<String> {
        std::env::var(&self.device_key_env).with_context(|| {
            format!(
                "source '{}': env var {} is not set",
                self.name, self.device_key_env
            )
        })
    }
}

impl AgentConfig {
    pub(crate) fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading agent config '{path}'"))?;
        let mut cfg: AgentConfig =
            toml::from_str(&text).with_context(|| format!("parsing agent config '{path}'"))?;
        if cfg.sources.is_empty() {
            bail!("agent config '{path}' declares no [[sources]]");
        }
        for s in &mut cfg.sources {
            s.url = s.url.trim_end_matches('/').to_string();
            s.key()?; // fail at startup, not on the first lease
        }
        Ok(cfg)
    }
}

fn default_device() -> String {
    "default".to_string()
}

fn default_poll_secs() -> u64 {
    15
}

fn default_lease_secs() -> i64 {
    1800
}

fn default_max_batch() -> usize {
    3
}

fn default_actions_dir() -> String {
    "actions".to_string()
}

fn default_claude_bin() -> String {
    "claude".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_applies_defaults_and_requires_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.toml");

        std::fs::write(&path, "sources = []").unwrap();
        assert!(AgentConfig::load(path.to_str().unwrap()).is_err());

        std::env::set_var("LT_TEST_DEVICE_KEY", "k");
        std::fs::write(
            &path,
            "[[sources]]\nname = \"cloud\"\nurl = \"https://x.example/\"\ndevice_key_env = \"LT_TEST_DEVICE_KEY\"\n",
        )
        .unwrap();
        let cfg = AgentConfig::load(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.device, "default");
        assert_eq!(cfg.poll_secs, 15);
        assert_eq!(cfg.sources[0].url, "https://x.example"); // trailing slash trimmed
        assert_eq!(cfg.sources[0].key().unwrap(), "k");
    }
}
