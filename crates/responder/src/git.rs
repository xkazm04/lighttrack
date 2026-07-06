//! Thin async git helpers over `git -C <repo> ...` for the auto-fix stage. Best-effort: each returns
//! a plain bool/Option so the actor can bail cleanly rather than leave the repo in a surprising state.

use std::process::Output;

use tokio::process::Command;

async fn git(repo: &str, args: &[&str]) -> std::io::Result<Output> {
    Command::new("git").arg("-C").arg(repo).args(args).output().await
}

fn ok(r: std::io::Result<Output>) -> bool {
    r.map(|o| o.status.success()).unwrap_or(false)
}

/// True when the working tree has no changes (tracked or untracked).
pub(crate) async fn is_clean(repo: &str) -> bool {
    match git(repo, &["status", "--porcelain"]).await {
        Ok(o) => o.status.success() && o.stdout.is_empty(),
        Err(_) => false,
    }
}

pub(crate) async fn has_changes(repo: &str) -> bool {
    !is_clean(repo).await
}

pub(crate) async fn current_branch(repo: &str) -> Option<String> {
    let o = git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).await.ok()?;
    if !o.status.success() {
        return None;
    }
    let b = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if b.is_empty() {
        None
    } else {
        Some(b)
    }
}

pub(crate) async fn checkout_new(repo: &str, branch: &str) -> bool {
    ok(git(repo, &["checkout", "-b", branch]).await)
}

pub(crate) async fn checkout(repo: &str, branch: &str) -> bool {
    ok(git(repo, &["checkout", branch]).await)
}

pub(crate) async fn add_commit(repo: &str, msg: &str) -> bool {
    if !ok(git(repo, &["add", "-A"]).await) {
        return false;
    }
    ok(git(repo, &["commit", "-m", msg]).await)
}
