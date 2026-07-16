//! Responder configuration: service settings from env + the project→repo map from a JSON file.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::email::EmailConfig;

pub(crate) struct Config {
    pub bind: String,
    pub lighttrack_url: String,
    pub claude_bin: String,
    pub report_dir: String,
    pub defaults: Defaults,
    pub projects: HashMap<String, ProjectEntry>,
    /// Optional email delivery of finished diagnoses (Resend).
    pub email: Option<EmailConfig>,
}

pub(crate) struct Defaults {
    pub model: String,
    pub permission_mode: String,
    pub max_budget_usd: f64,
    pub enrich_limit: usize,
    /// Wall-clock cap on one investigation; the claude child is killed past it. This CLI has no
    /// `--max-turns`, so the timeout is the only hard bound on a runaway (over-)exploration.
    pub timeout_secs: u64,
    /// Permission mode for the ACT (auto-fix) run — `acceptEdits` lets it edit files without asking.
    pub act_permission_mode: String,
    /// Circuit breaker: minimum seconds between auto-fixes on the same project.
    pub act_cooldown_secs: u64,
    /// Circuit breaker: max auto-fixes applied across all projects per rolling hour.
    pub max_acts_per_hour: u32,
    /// Admission control (INVESTIGATE stage): minimum seconds between investigations of the same
    /// project. A flap fires the same spike repeatedly; this stops each one buying a fresh paid run.
    pub investigate_cooldown_secs: u64,
    /// Admission control: max investigations spawned across all projects per rolling hour — a hard
    /// spend ceiling independent of the (remote, in-another-process) alerter cooldown.
    pub max_investigations_per_hour: u32,
    /// Admission control: max investigations running concurrently. Shed (log + drop) past this rather
    /// than queue — a stale spike is not worth a queued paid run.
    pub max_concurrent_investigations: usize,
}

/// One mapped project: where its code lives locally, plus optional hints for the investigator.
#[derive(Deserialize, Clone)]
pub(crate) struct ProjectEntry {
    pub repo: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub hint: Option<String>,
    #[serde(default)]
    pub test_cmd: Option<String>,
    /// Opt-in: allow the ACT stage to auto-apply a fix on a branch for this project. Default off.
    #[serde(default)]
    pub auto_fix: bool,
}

impl Config {
    pub(crate) fn from_env() -> anyhow::Result<Self> {
        let map_path = env_or("LIGHTTRACK_RESPONDER_MAP", "responder.map.json");
        let (mut defaults, projects) = load_map(&map_path);
        // Env override for the one setting we tune per-run during testing.
        if let Some(t) = env_opt("LIGHTTRACK_RESPONDER_TIMEOUT_SECS").and_then(|s| s.parse().ok()) {
            defaults.timeout_secs = t;
        }
        Ok(Config {
            bind: env_or("LIGHTTRACK_RESPONDER_BIND", "127.0.0.1:8790"),
            lighttrack_url: env_or("LIGHTTRACK_URL", "http://127.0.0.1:8787"),
            claude_bin: env_opt("LIGHTTRACK_RESPONDER_CLAUDE_BIN")
                .unwrap_or_else(|| resolve_claude_bin("claude")),
            report_dir: env_or("LIGHTTRACK_RESPONDER_REPORT_DIR", "diagnoses"),
            defaults,
            projects,
            email: EmailConfig::from_env(),
        })
    }
}

#[derive(Deserialize, Default)]
struct RawDefaults {
    model: Option<String>,
    permission_mode: Option<String>,
    max_budget_usd: Option<f64>,
    enrich_limit: Option<usize>,
    timeout_secs: Option<u64>,
    act_permission_mode: Option<String>,
    act_cooldown_secs: Option<u64>,
    max_acts_per_hour: Option<u32>,
    investigate_cooldown_secs: Option<u64>,
    max_investigations_per_hour: Option<u32>,
    max_concurrent_investigations: Option<usize>,
}

#[derive(Deserialize, Default)]
struct MapFile {
    #[serde(default)]
    defaults: RawDefaults,
    #[serde(default)]
    projects: HashMap<String, ProjectEntry>,
}

/// Load the map file, falling back to built-in defaults + an empty project set when it is missing or
/// malformed (the service still starts and serves `/health`; unmapped spikes are simply skipped).
fn load_map(path: &str) -> (Defaults, HashMap<String, ProjectEntry>) {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("[responder] map file '{path}' not found; starting with no projects mapped");
            return (Defaults::fallback(), HashMap::new());
        }
    };
    let parsed: MapFile = match serde_json::from_str(&raw) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[responder] map file '{path}' is not valid JSON ({e}); ignoring it");
            return (Defaults::fallback(), HashMap::new());
        }
    };
    let d = parsed.defaults;
    let defaults = Defaults {
        model: d.model.unwrap_or_else(|| "sonnet".to_string()),
        permission_mode: d.permission_mode.unwrap_or_else(|| "default".to_string()),
        max_budget_usd: d.max_budget_usd.unwrap_or(1.0),
        enrich_limit: d.enrich_limit.unwrap_or(20),
        timeout_secs: d.timeout_secs.unwrap_or(240),
        act_permission_mode: d.act_permission_mode.unwrap_or_else(|| "acceptEdits".to_string()),
        act_cooldown_secs: d.act_cooldown_secs.unwrap_or(3600),
        max_acts_per_hour: d.max_acts_per_hour.unwrap_or(3),
        investigate_cooldown_secs: d.investigate_cooldown_secs.unwrap_or(600),
        max_investigations_per_hour: d.max_investigations_per_hour.unwrap_or(20),
        max_concurrent_investigations: d.max_concurrent_investigations.unwrap_or(2),
    };
    (defaults, parsed.projects)
}

impl Defaults {
    fn fallback() -> Self {
        Defaults {
            model: "sonnet".to_string(),
            permission_mode: "default".to_string(),
            max_budget_usd: 1.0,
            enrich_limit: 20,
            timeout_secs: 240,
            act_permission_mode: "acceptEdits".to_string(),
            act_cooldown_secs: 3600,
            max_acts_per_hour: 3,
            investigate_cooldown_secs: 600,
            max_investigations_per_hour: 20,
            max_concurrent_investigations: 2,
        }
    }
}

/// Resolve a runnable claude executable. Mirrors `lighttrack_engine::resolve_claude_bin` but is kept
/// local so the responder doesn't pull in the engine's generation/judge stack. A child process can't
/// invoke the npm `.cmd`/`.ps1` shims, so on Windows we prefer a real `claude.exe`.
fn resolve_claude_bin(given: &str) -> String {
    if given != "claude" {
        return given.to_string();
    }
    #[cfg(windows)]
    {
        // Native installer (`~/.local/bin`) first, then a global npm install.
        if let Ok(home) = std::env::var("USERPROFILE") {
            let p = format!("{home}\\.local\\bin\\claude.exe");
            if Path::new(&p).exists() {
                return p;
            }
        }
        if let Ok(appdata) = std::env::var("APPDATA") {
            let p =
                format!("{appdata}\\npm\\node_modules\\@anthropic-ai\\claude-code\\bin\\claude.exe");
            if Path::new(&p).exists() {
                return p;
            }
        }
    }
    given.to_string()
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| default.to_string())
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}
