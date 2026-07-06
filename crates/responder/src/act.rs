//! The ACT stage: gated auto-fix. Opt-in per project (`auto_fix`), it works on a fresh `lt-fix/*`
//! branch cut from HEAD, applies a minimal fix via Claude Code (acceptEdits), runs the project's test
//! command, then **restores the original branch** — so the user's checkout is never left changed, the
//! fix lands on a review branch (never main, never pushed), and a runaway is bounded by the breaker.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::breaker::Breaker;
use crate::claude;
use crate::config::{Config, ProjectEntry};
use crate::git;
use crate::webhook::Spike;

pub(crate) struct ActOutcome {
    pub skipped_reason: Option<String>,
    pub branch: Option<String>,
    pub applied: bool,
    pub tests: Option<bool>,
    pub notes: String,
    pub cost_usd: Option<f64>,
}

impl ActOutcome {
    fn skip(reason: impl Into<String>) -> Self {
        ActOutcome {
            skipped_reason: Some(reason.into()),
            branch: None,
            applied: false,
            tests: None,
            notes: String::new(),
            cost_usd: None,
        }
    }
}

pub(crate) async fn run_act(
    cfg: &Config,
    breaker: &Breaker,
    entry: &ProjectEntry,
    spike: &Spike,
    diagnosis: &str,
    ts: &str,
) -> ActOutcome {
    if !entry.auto_fix {
        return ActOutcome::skip("auto_fix disabled for this project");
    }
    if !git::is_clean(&entry.repo).await {
        return ActOutcome::skip("working tree not clean — refusing to auto-edit");
    }
    let orig = match git::current_branch(&entry.repo).await {
        Some(b) => b,
        None => return ActOutcome::skip("not a git repo / could not read current branch"),
    };
    if let Err(why) = breaker.allow(
        &spike.project_id,
        Duration::from_secs(cfg.defaults.act_cooldown_secs),
        cfg.defaults.max_acts_per_hour,
    ) {
        return ActOutcome::skip(format!("circuit breaker: {why}"));
    }

    let safe: String = spike
        .project_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    let branch = format!("lt-fix/{safe}-{ts}");
    if !git::checkout_new(&entry.repo, &branch).await {
        return ActOutcome::skip(format!("git checkout -b {branch} failed"));
    }

    let prompt = build_fix_prompt(entry, spike, diagnosis);
    let run = claude::run(cfg, &entry.repo, &cfg.defaults.act_permission_mode, &prompt).await;

    let applied = git::has_changes(&entry.repo).await;
    let mut tests = None;
    let notes;
    if applied {
        breaker.record(&spike.project_id);
        git::add_commit(&entry.repo, &format!("lt-responder auto-fix: {} ({ts})", spike.project_id))
            .await;
        if let Some(cmd) = &entry.test_cmd {
            tests = Some(run_test(&entry.repo, cmd, cfg.defaults.timeout_secs).await);
        }
        notes = run.text;
    } else {
        notes = format!("no changes applied (claude judged no confident fix):\n\n{}", run.text);
    }

    // Always put the user's working copy back the way we found it.
    git::checkout(&entry.repo, &orig).await;

    ActOutcome {
        skipped_reason: None,
        branch: Some(branch),
        applied,
        tests,
        notes,
        cost_usd: run.cost_usd,
    }
}

async fn run_test(repo: &str, test_cmd: &str, timeout_secs: u64) -> bool {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(test_cmd);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(test_cmd);
        c
    };
    cmd.current_dir(repo).stdin(Stdio::null()).kill_on_drop(true);
    matches!(
        tokio::time::timeout(Duration::from_secs(timeout_secs), cmd.output()).await,
        Ok(Ok(o)) if o.status.success()
    )
}

fn build_fix_prompt(entry: &ProjectEntry, spike: &Spike, diagnosis: &str) -> String {
    let verify = entry.test_cmd.as_deref().unwrap_or("(none configured)");
    format!(
        "You are applying a fix for a production LLM failure in project '{project}', which is the \
         current working directory. A prior read-only investigation produced this diagnosis:\n\n\
         --- DIAGNOSIS ---\n{diagnosis}\n--- END DIAGNOSIS ---\n\n\
         Apply a MINIMAL fix for the root cause IF — and only if — you are confident it is correct. \
         If you are not confident, make NO file changes and explain why instead.\n\
         Rules: do not refactor unrelated code; do not weaken or edit tests to force a pass; do not \
         run any git commands; do not push. After editing, briefly summarize what you changed and \
         why. The project's test command is '{verify}' (it will be run for you — you need not run it).",
        project = spike.project_id,
    )
}
