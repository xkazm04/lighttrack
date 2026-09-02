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

use super::envelope::split_effort;
use super::tools::{allowlist, is_readonly_tool};
use super::{Invocation, Mode};
use crate::{EngineError, Result};

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
    let tools = allowlist(inv);

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
mod tests;
