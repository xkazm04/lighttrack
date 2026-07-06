//! Shared `claude -p` runner: spawn Claude Code headless in a repo, bounded by `max_budget_usd` and a
//! wall-clock timeout (this CLI has no `--max-turns`), and parse the JSON result envelope. Used by
//! both the read-only investigator and the auto-fix actor — they differ only in permission mode and
//! prompt.

use std::process::Stdio;

use serde_json::Value;
use tokio::process::Command;

use crate::config::Config;

pub(crate) struct ClaudeRun {
    pub text: String,
    pub model: String,
    pub cost_usd: Option<f64>,
    pub ok: bool,
}

pub(crate) async fn run(
    cfg: &Config,
    repo: &str,
    permission_mode: &str,
    allowed_tools: &[&str],
    prompt: &str,
) -> ClaudeRun {
    let mut cmd = Command::new(&cfg.claude_bin);
    cmd.arg("-p")
        .arg(prompt)
        .arg("--model")
        .arg(&cfg.defaults.model)
        .arg("--permission-mode")
        .arg(permission_mode)
        .arg("--output-format")
        .arg("json")
        .arg("--max-budget-usd")
        .arg(format!("{:.2}", cfg.defaults.max_budget_usd));
    // `--allowedTools` is variadic, so it must come LAST or it swallows later flags. An allowlist
    // (read-only for the investigator) keeps the run read-only WITHOUT plan mode — plan mode makes
    // Claude write its analysis to a plan file and return only a terse note, losing the diagnosis.
    if !allowed_tools.is_empty() {
        cmd.arg("--allowedTools");
        for t in allowed_tools {
            cmd.arg(t);
        }
    }
    cmd.current_dir(repo).stdin(Stdio::null()).kill_on_drop(true);

    let dur = std::time::Duration::from_secs(cfg.defaults.timeout_secs);
    let output = match tokio::time::timeout(dur, cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return fail(cfg, format!("failed to launch claude ('{}'): {e}", cfg.claude_bin)),
        Err(_) => {
            return fail(
                cfg,
                format!("claude run timed out after {}s and was killed.", cfg.defaults.timeout_secs),
            )
        }
    };

    // `--output-format json` prints a result envelope on stdout even on a controlled non-zero exit
    // (e.g. the budget cap), so parse stdout regardless and read `is_error` for the real verdict.
    let exit_ok = output.status.success();
    let stderr = String::from_utf8_lossy(&output.stderr);
    match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(env) => {
            let is_error = env.get("is_error").and_then(Value::as_bool).unwrap_or(!exit_ok);
            let subtype = env.get("subtype").and_then(Value::as_str).unwrap_or("");
            let result = env.get("result").and_then(Value::as_str).unwrap_or("");
            let text = match (is_error, result.is_empty()) {
                (false, _) => result.to_string(),
                (true, false) => format!("[claude reported an error: subtype={subtype}]\n\n{result}"),
                (true, true) => format!(
                    "claude returned an error (subtype={subtype}, exit={}). stderr:\n{}",
                    output.status.code().unwrap_or(-1),
                    stderr.trim()
                ),
            };
            ClaudeRun {
                text,
                model: env
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or(&cfg.defaults.model)
                    .to_string(),
                cost_usd: env.get("total_cost_usd").and_then(Value::as_f64),
                ok: exit_ok && !is_error,
            }
        }
        Err(e) => fail(
            cfg,
            format!(
                "claude exited with {} and produced no JSON envelope ({e}).\nstdout:\n{}\nstderr:\n{}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stdout).trim(),
                stderr.trim()
            ),
        ),
    }
}

fn fail(cfg: &Config, text: String) -> ClaudeRun {
    ClaudeRun { text, model: cfg.defaults.model.clone(), cost_usd: None, ok: false }
}
