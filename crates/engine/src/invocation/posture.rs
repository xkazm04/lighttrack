//! Posture: what a given [`Mode`] is *allowed* to be, and the argv that expresses it.
//!
//! This is the whole point of the seam. A headless Claude run's blast radius is decided by four
//! things — the tool allowlist, the working directory, the permission mode, and whether the billing
//! key is in the child's environment. Spread across three call sites those four drifted apart; here
//! they are one function, and a caller that asks for a contradiction (an edit run with no
//! workspace, a "read-only" scan that allows `Write`) gets [`EngineError::Posture`] *before* a
//! child exists, not a surprising diff afterwards.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Once;

use serde::Deserialize;

use super::envelope::split_effort;
use super::Invocation;
use crate::{EngineError, Result};

/// What a run is for — and therefore what it may touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// A completion, nothing more: no tools, no repository, no permission mode. Judging, candidate
    /// generation, and the default for a relay action.
    #[default]
    Generate,
    /// An agentic run that may look but not touch: the read-only tool allowlist plus whatever
    /// extras the caller names, all of which must themselves be read-only.
    ReadonlyScan,
    /// An agentic run that may edit files. Requires an explicit workspace and permission mode —
    /// there is no default that is safe enough to be implicit.
    Edit,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Generate => "generate",
            Mode::ReadonlyScan => "readonly-scan",
            Mode::Edit => "edit",
        }
    }
}

impl std::str::FromStr for Mode {
    type Err = EngineError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "generate" => Ok(Mode::Generate),
            "readonly-scan" => Ok(Mode::ReadonlyScan),
            "edit" => Ok(Mode::Edit),
            other => Err(EngineError::Posture(format!(
                "unknown mode '{other}' (expected generate|readonly-scan|edit)"
            ))),
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The tools a [`Mode::ReadonlyScan`] always gets. Anything else must be named by the caller and
/// must pass [`is_readonly_tool`].
pub const READONLY_BASE_TOOLS: &[&str] = &["Read", "Glob", "Grep", "LS"];

/// Read-only `Bash(...)` prefixes. The allowlist is a prefix match on the command inside the
/// parentheses, so `Bash(git log:*)` is fine and `Bash(git push:*)` is not.
const READONLY_BASH_PREFIXES: &[&str] = &[
    "git log",
    "git diff",
    "git show",
    "git status",
    "git blame",
    "git ls-files",
    "ls",
    "cat",
    "head",
    "tail",
    "wc",
    "rg",
    "grep",
    "find",
];

/// Non-`Bash` tools that read but never write.
const READONLY_TOOL_NAMES: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "LS",
    "NotebookRead",
    "WebFetch",
    "WebSearch",
];

/// A resolved plan: exactly what [`run`](super::run) will spawn.
#[derive(Debug)]
pub(crate) struct Plan {
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// A non-zero exit is a hard error rather than an envelope to read. True only for
    /// [`Mode::Generate`]: the agentic modes pass `--max-budget-usd`, whose enforcement is a
    /// *controlled* non-zero exit that still prints a result envelope worth reading.
    pub strict_exit: bool,
}

/// Validate the invocation against its mode and render the argv. The single place the rules live.
pub(crate) fn plan(inv: &Invocation<'_>) -> Result<Plan> {
    let cwd = check(inv)?;
    let tools = tools_for(inv);

    let (model, suffix_effort) = split_effort(inv.model);
    let effort = inv.effort.or(suffix_effort);

    let mut args = vec![
        // No prompt argument: it travels over stdin. Windows caps a command line at ~32k chars and
        // quoting a judge prompt through a shell is a bug farm; stdin has neither limit nor quoting.
        "-p".to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--model".to_string(),
        model.to_string(),
    ];
    if let Some(level) = effort {
        args.push("--effort".to_string());
        args.push(level.to_string());
    }
    if let Some(sys) = inv.system {
        args.push("--append-system-prompt".to_string());
        args.push(sys.to_string());
    }
    if let Some(schema) = inv.schema {
        args.push("--json-schema".to_string());
        args.push(schema.to_string());
    }
    if let Some(mode) = inv.permission_mode {
        args.push("--permission-mode".to_string());
        args.push(mode.to_string());
    }
    if let Some(budget) = inv.budget_usd {
        args.push("--max-budget-usd".to_string());
        args.push(format!("{budget:.2}"));
    }
    if inv.bare {
        args.push("--bare".to_string());
    }
    // `--allowedTools` is variadic, so it must come LAST or it swallows every later flag.
    if !tools.is_empty() {
        args.push("--allowedTools".to_string());
        args.extend(tools);
    }

    Ok(Plan {
        args,
        cwd,
        strict_exit: inv.mode == Mode::Generate,
    })
}

/// Enforce the mode's contract and resolve the working directory.
fn check(inv: &Invocation<'_>) -> Result<PathBuf> {
    if let Some(effort) = inv.effort {
        if !matches!(effort, "low" | "medium" | "high" | "xhigh" | "max") {
            return Err(EngineError::Posture(format!(
                "unknown effort '{effort}' (expected low|medium|high|xhigh|max)"
            )));
        }
    }
    if inv.budget_usd.is_some_and(|b| b <= 0.0 || b.is_nan()) {
        return Err(EngineError::Posture(
            "budget_usd must be positive when set".to_string(),
        ));
    }
    match inv.mode {
        Mode::Generate => {
            if !inv.allowed_tools.is_empty() {
                return Err(EngineError::Posture(format!(
                    "generate runs take no tools, got {:?}",
                    inv.allowed_tools
                )));
            }
            if let Some(mode) = inv.permission_mode {
                return Err(EngineError::Posture(format!(
                    "generate runs take no permission mode, got '{mode}'"
                )));
            }
            if let Some(cwd) = &inv.cwd {
                return Err(EngineError::Posture(format!(
                    "generate runs take no workspace, got '{}'",
                    cwd.display()
                )));
            }
            neutral_cwd()
        }
        Mode::ReadonlyScan => {
            for tool in &inv.allowed_tools {
                if !is_readonly_tool(tool) {
                    return Err(EngineError::Posture(format!(
                        "readonly-scan cannot allow '{tool}' — it can write"
                    )));
                }
            }
            match inv.permission_mode {
                None | Some("plan") | Some("default") => {}
                Some(other) => {
                    return Err(EngineError::Posture(format!(
                        "readonly-scan permission mode must be plan|default, got '{other}'"
                    )))
                }
            }
            // A scan without a workspace would read whatever the parent happened to be sitting in.
            let cwd = inv.cwd.clone().ok_or_else(|| {
                EngineError::Posture("readonly-scan requires an explicit workspace".to_string())
            })?;
            Ok(cwd)
        }
        Mode::Edit => {
            let cwd = inv.cwd.clone().ok_or_else(|| {
                EngineError::Posture("edit runs require an explicit workspace".to_string())
            })?;
            let mode = inv.permission_mode.ok_or_else(|| {
                EngineError::Posture(
                    "edit runs require an explicit permission mode (e.g. acceptEdits)".to_string(),
                )
            })?;
            if mode.is_empty() {
                return Err(EngineError::Posture(
                    "edit runs require a non-empty permission mode".to_string(),
                ));
            }
            Ok(cwd)
        }
    }
}

/// The final tool allowlist for this mode: the base set plus the caller's extras, deduplicated and
/// order-stable so the argv is comparable across runs (and testable).
fn tools_for(inv: &Invocation<'_>) -> Vec<String> {
    if inv.mode == Mode::Generate {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    if inv.mode == Mode::ReadonlyScan {
        out.extend(READONLY_BASE_TOOLS.iter().map(|t| t.to_string()));
    }
    for t in &inv.allowed_tools {
        if !out.iter().any(|x| x == t) {
            out.push(t.clone());
        }
    }
    out
}

/// Whether a tool spec can only read. Everything not recognised is treated as write-capable — the
/// safe direction for an allowlist is to reject what we don't understand.
pub(crate) fn is_readonly_tool(tool: &str) -> bool {
    if let Some(rest) = tool.strip_prefix("Bash(") {
        let inner = rest.trim_end_matches(')').trim_end_matches(":*").trim();
        return READONLY_BASH_PREFIXES.iter().any(|p| inner.starts_with(p));
    }
    READONLY_TOOL_NAMES.contains(&tool)
}

/// A directory with no ambient project context. A `Generate` run is a pure completion, so it must
/// not inherit whatever `CLAUDE.md`, hooks or settings happen to sit in the caller's cwd — that
/// context would silently join the prompt and make the same judge call mean different things in
/// different checkouts.
fn neutral_cwd() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("lighttrack-claude-neutral");
    std::fs::create_dir_all(&dir).map_err(|e| {
        EngineError::Other(format!(
            "creating neutral working directory '{}': {e}",
            dir.display()
        ))
    })?;
    Ok(dir)
}

static AUTH_LOG: Once = Once::new();

/// `subscription-auth-selection`: one decision about the billing key, logged once per process.
///
/// A seat run (`bare == false`) authenticates through the CLI's subscription OAuth. If
/// `ANTHROPIC_API_KEY` is in the environment the CLI may bill the API instead, silently converting
/// flat-rate work into metered spend — so the key is *stripped* from the child. `--bare` skips the
/// auto-loaded context and with it the OAuth path, so there the key is *required*.
pub(crate) fn apply_auth(cmd: &mut Command, bare: bool) -> Result<()> {
    const KEY: &str = "ANTHROPIC_API_KEY";
    let present = std::env::var_os(KEY).is_some_and(|v| !v.is_empty());
    if bare {
        if !present {
            return Err(EngineError::Posture(format!(
                "--bare bypasses subscription OAuth, so it requires {KEY} in the environment"
            )));
        }
        AUTH_LOG.call_once(|| eprintln!("[engine] claude auth: --bare, billing to {KEY}"));
    } else {
        cmd.env_remove(KEY);
        AUTH_LOG.call_once(|| {
            let note = if present {
                " (stripped from the child)"
            } else {
                ""
            };
            eprintln!("[engine] claude auth: seat/subscription OAuth; {KEY} unused{note}");
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(inv: &Invocation<'_>) -> Vec<String> {
        plan(inv).expect("posture should accept").args
    }

    fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
        let i = args.iter().position(|a| a == name)?;
        args.get(i + 1).map(String::as_str)
    }

    /// The posture matrix: one assertion block per mode over the argv it produces.
    #[test]
    fn posture_matrix_pins_each_mode_argv() {
        // Generate: no tools, no permission mode, a neutral cwd, prompt never on argv.
        let inv = Invocation::generate("judge this", "opus@xhigh");
        let p = plan(&inv).unwrap();
        assert_eq!(flag(&p.args, "--model"), Some("opus"));
        assert_eq!(flag(&p.args, "--effort"), Some("xhigh"));
        assert_eq!(flag(&p.args, "--output-format"), Some("json"));
        assert!(!p.args.iter().any(|a| a == "--allowedTools"));
        assert!(!p.args.iter().any(|a| a == "--permission-mode"));
        assert!(!p.args.iter().any(|a| a == "judge this"));
        assert!(p.strict_exit);
        assert_ne!(p.cwd, std::env::current_dir().unwrap());

        // ReadonlyScan: base allowlist + extras, last, and a controlled non-zero exit is readable.
        let inv = Invocation::readonly_scan("look", "sonnet")
            .with_cwd("/repo")
            .with_allowed_tools(["Bash(git log:*)"])
            .with_permission_mode(Some("default"))
            .with_budget_usd(Some(1.0));
        let p = plan(&inv).unwrap();
        let at = p.args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(
            &p.args[at + 1..],
            &["Read", "Glob", "Grep", "LS", "Bash(git log:*)"]
        );
        assert_eq!(at + 6, p.args.len(), "--allowedTools must be last");
        assert_eq!(flag(&p.args, "--permission-mode"), Some("default"));
        assert_eq!(flag(&p.args, "--max-budget-usd"), Some("1.00"));
        assert!(!p.strict_exit);

        // Edit: workspace + permission mode carried through; no base allowlist forced on it.
        let inv = Invocation::edit("fix", "sonnet")
            .with_cwd("/repo")
            .with_permission_mode(Some("acceptEdits"));
        let p = plan(&inv).unwrap();
        assert_eq!(flag(&p.args, "--permission-mode"), Some("acceptEdits"));
        assert_eq!(p.cwd, PathBuf::from("/repo"));
        assert!(!p.args.iter().any(|a| a == "--allowedTools"));
    }

    #[test]
    fn readonly_scan_never_carries_a_write_tool() {
        for bad in [
            "Write",
            "Edit",
            "Bash(git push:*)",
            "Bash(rm -rf:*)",
            "Task",
        ] {
            let inv = Invocation::readonly_scan("x", "sonnet")
                .with_cwd("/repo")
                .with_allowed_tools([bad]);
            let err = plan(&inv).expect_err("write tool must be rejected");
            assert!(matches!(err, EngineError::Posture(_)), "{bad}: {err:?}");
        }
        // …and the tools it does emit are all read-only, base set included.
        let inv = Invocation::readonly_scan("x", "sonnet")
            .with_cwd("/repo")
            .with_allowed_tools(["Bash(git diff:*)", "WebFetch"]);
        for tool in tools_for(&inv) {
            assert!(is_readonly_tool(&tool), "{tool} is not read-only");
        }
    }

    #[test]
    fn contradictions_fail_before_spawning() {
        let cases: Vec<(&str, Invocation<'_>)> = vec![
            (
                "generate + tools",
                Invocation::generate("x", "m").with_allowed_tools(["Read"]),
            ),
            (
                "generate + cwd",
                Invocation::generate("x", "m").with_cwd("/repo"),
            ),
            (
                "generate + permission mode",
                Invocation::generate("x", "m").with_permission_mode(Some("plan")),
            ),
            ("edit without cwd", Invocation::edit("x", "m")),
            (
                "edit without permission mode",
                Invocation::edit("x", "m").with_cwd("/repo"),
            ),
            ("scan without cwd", Invocation::readonly_scan("x", "m")),
            (
                "scan in acceptEdits",
                Invocation::readonly_scan("x", "m")
                    .with_cwd("/repo")
                    .with_permission_mode(Some("acceptEdits")),
            ),
            (
                "bad effort",
                Invocation::generate("x", "m").with_effort(Some("turbo")),
            ),
            (
                "non-positive budget",
                Invocation::edit("x", "m")
                    .with_cwd("/repo")
                    .with_permission_mode(Some("acceptEdits"))
                    .with_budget_usd(Some(0.0)),
            ),
        ];
        for (name, inv) in cases {
            match plan(&inv) {
                Err(EngineError::Posture(_)) => {}
                other => panic!("{name}: expected a posture error, got {other:?}"),
            }
        }
    }

    #[test]
    fn schema_system_and_bare_reach_the_argv() {
        let inv = Invocation::generate("x", "haiku")
            .with_system(Some("be terse"))
            .with_schema(Some("{\"type\":\"object\"}"))
            .with_bare(true);
        let args = args_of(&inv);
        assert_eq!(flag(&args, "--append-system-prompt"), Some("be terse"));
        assert_eq!(flag(&args, "--json-schema"), Some("{\"type\":\"object\"}"));
        assert!(args.iter().any(|a| a == "--bare"));
    }

    #[test]
    fn mode_round_trips_through_its_wire_name() {
        for m in [Mode::Generate, Mode::ReadonlyScan, Mode::Edit] {
            assert_eq!(m.as_str().parse::<Mode>().unwrap(), m);
            assert_eq!(
                serde_json::from_value::<Mode>(serde_json::json!(m.as_str())).unwrap(),
                m
            );
        }
        assert!("acceptEdits".parse::<Mode>().is_err());
        assert_eq!(Mode::default(), Mode::Generate);
    }

    #[test]
    fn seat_runs_strip_the_billing_key_and_bare_runs_demand_it() {
        // The env var is process-global; drive both branches off an explicit `bare` flag and a
        // key we set/clear here rather than whatever the developer's shell happens to hold.
        let mut cmd = Command::new("echo");
        std::env::set_var("ANTHROPIC_API_KEY", "sk-test");
        assert!(apply_auth(&mut cmd, false).is_ok());
        assert!(apply_auth(&mut cmd, true).is_ok());
        std::env::remove_var("ANTHROPIC_API_KEY");
        assert!(apply_auth(&mut cmd, false).is_ok());
        assert!(matches!(
            apply_auth(&mut cmd, true),
            Err(EngineError::Posture(_))
        ));
    }
}
