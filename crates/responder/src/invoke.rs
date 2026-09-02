//! The responder's adapter onto the engine's one Claude invocation seam
//! (`lighttrack_engine::invocation`).
//!
//! The responder used to build its own `Command` — its own answer to which tools a run may call,
//! which directory it sees, and how long it may hang. It no longer does: `investigate` is a
//! [`Mode::ReadonlyScan`] and `act` is a [`Mode::Edit`], and the difference between them is
//! declared, not spelled out in flags. The seam is blocking, so the async pipeline runs it on a
//! blocking task.

use lighttrack_engine::invocation::{self, ClaudeBin, Invocation, Mode};

use crate::config::Config;

/// What a finished run tells the pipeline. Deliberately narrow — the report and ACT stages only
/// ever needed the text, the model, the money, and whether it worked.
pub(crate) struct ClaudeRun {
    pub text: String,
    pub model: String,
    pub cost_usd: Option<f64>,
    pub ok: bool,
}

/// Run Claude Code in `repo` under an explicit posture. `extra_tools` is meaningful only for a
/// read-only scan (the seam rejects a write tool there); an edit run carries none and relies on its
/// permission mode.
pub(crate) async fn run(
    cfg: &Config,
    repo: &str,
    mode: Mode,
    permission_mode: &str,
    extra_tools: &[&str],
    prompt: &str,
) -> ClaudeRun {
    let bin = ClaudeBin::new(cfg.claude_bin.clone());
    let model = cfg.defaults.model.clone();
    let repo = repo.to_string();
    let permission_mode = permission_mode.to_string();
    let tools: Vec<String> = extra_tools.iter().map(|t| t.to_string()).collect();
    let prompt = prompt.to_string();
    let budget = cfg.defaults.max_budget_usd;
    // This CLI has no `--max-turns`, so the wall clock is the only hard bound on a runaway.
    let timeout = std::time::Duration::from_secs(cfg.defaults.timeout_secs);
    let timeout_secs = cfg.defaults.timeout_secs;

    let out = tokio::task::spawn_blocking(move || {
        let inv = Invocation::with_mode(&prompt, &model, mode)
            .with_cwd(&repo)
            .with_permission_mode(Some(&permission_mode))
            .with_allowed_tools(tools)
            .with_budget_usd(Some(budget))
            .with_timeout(timeout);
        invocation::run(&bin, &inv)
    })
    .await;

    let fallback = cfg.defaults.model.clone();
    match out {
        Ok(Ok(out)) if out.ok() => ClaudeRun {
            text: out.text,
            model: out.model,
            cost_usd: out.cost_usd,
            ok: true,
        },
        // A well-formed envelope that reports an error is a *controlled* failure — the budget cap
        // is the common one — and its text is the only place it explains itself, so it is kept.
        Ok(Ok(out)) => ClaudeRun {
            text: describe(&out),
            model: out.model,
            cost_usd: out.cost_usd,
            ok: false,
        },
        Ok(Err(e)) => fail(
            fallback,
            match e {
                lighttrack_engine::EngineError::Timeout { .. } => {
                    format!("claude run timed out after {timeout_secs}s and was killed.")
                }
                other => format!("claude run failed: {other}"),
            },
        ),
        Err(e) => fail(fallback, format!("claude run task panicked: {e}")),
    }
}

fn describe(out: &lighttrack_engine::RawOutcome) -> String {
    let subtype = &out.subtype;
    if out.text.is_empty() {
        format!(
            "claude returned an error (subtype={subtype}, exit_ok={}). stderr:\n{}",
            out.exit_ok, out.stderr
        )
    } else {
        format!(
            "[claude reported an error: subtype={subtype}]\n\n{}",
            out.text
        )
    }
}

fn fail(model: String, text: String) -> ClaudeRun {
    ClaudeRun {
        text,
        model,
        cost_usd: None,
        ok: false,
    }
}
