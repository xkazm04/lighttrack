//! Thin async git helpers over `git -C <repo> ...` for the auto-fix stage. Best-effort: each returns
//! a plain bool/Option so the actor can bail cleanly rather than leave the repo in a surprising state.

use std::process::Output;

use tokio::process::Command;

/// Variables that locate a repository and silently override `-C`. Git exports `GIT_DIR` (and in a
/// worktree `GIT_INDEX_FILE`'s neighbours) to hooks, so a process started from a hook inherits
/// them: on 2026-09-19 the pre-push gate ran this module's tests from a worktree and `git -C
/// <scratch> init` reinitialized the REAL repository (`core.bare = true`), committed the scratch
/// "base" onto the pushed branch, and checked out `lt-fix/test` there.
const REPO_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_PREFIX",
];

async fn git(repo: &str, args: &[&str]) -> std::io::Result<Output> {
    let mut cmd = Command::new("git");
    for k in REPO_ENV {
        cmd.env_remove(k);
    }
    cmd.arg("-C").arg(repo).args(args).output().await
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
    let o = git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .ok()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway repository with one commit, isolated from the machine's git config so the test
    /// controls whether an identity exists.
    fn scratch_repo(name: &str) -> String {
        let dir =
            std::env::temp_dir().join(format!("lt-responder-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let run = |args: &[&str]| {
            let mut cmd = std::process::Command::new("git");
            for k in REPO_ENV {
                cmd.env_remove(k);
            }
            let out = cmd
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "base",
        ]);
        dir.to_string_lossy().into_owned()
    }

    /// The failure path ACT depends on being *detectable*: when git cannot commit (here: no identity
    /// anywhere), `add_commit` reports false and the tree stays dirty — which is exactly the state
    /// where restoring the original branch would smuggle the edits onto it.
    #[tokio::test]
    async fn a_failed_commit_reports_false_and_leaves_the_tree_dirty() {
        let repo = scratch_repo("nocommit");
        // No identity from any config level, and no fallback derived from the host.
        let empty = std::env::temp_dir().join(format!("lt-empty-gitconfig-{}", std::process::id()));
        std::fs::write(&empty, "").expect("empty config");
        std::env::set_var("GIT_CONFIG_GLOBAL", &empty);
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_CONFIG_COUNT", "1");
        std::env::set_var("GIT_CONFIG_KEY_0", "user.useConfigOnly");
        std::env::set_var("GIT_CONFIG_VALUE_0", "true");

        assert!(checkout_new(&repo, "lt-fix/test").await);
        std::fs::write(std::path::Path::new(&repo).join("fix.txt"), "edited").unwrap();
        assert!(has_changes(&repo).await, "the edit is visible");
        assert!(
            !add_commit(&repo, "auto-fix").await,
            "a commit with no identity must fail, and must say so"
        );
        assert!(
            has_changes(&repo).await,
            "…and the edits are still uncommitted on the fix branch"
        );
        assert_eq!(current_branch(&repo).await.as_deref(), Some("lt-fix/test"));

        for k in [
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
        ] {
            std::env::remove_var(k);
        }
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_file(&empty);
    }

    /// An inherited `GIT_DIR` (a hook's environment) must not redirect `-C <repo>` to another
    /// repository. Set on the child only, never on this process, so no sibling test can see it.
    #[test]
    fn an_inherited_git_dir_does_not_redirect_the_target_repo() {
        let target = scratch_repo("envtarget");
        let decoy = scratch_repo("envdecoy");
        let mut cmd = std::process::Command::new("git");
        cmd.env("GIT_DIR", std::path::Path::new(&decoy).join(".git"));
        for k in REPO_ENV {
            cmd.env_remove(k);
        }
        let out = cmd
            .arg("-C")
            .arg(&target)
            .args(["rev-parse", "--absolute-git-dir"])
            .output()
            .expect("git runs");
        let got = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success());
        assert!(got.contains("envtarget"), "resolved {got}");
        assert!(!got.contains("envdecoy"), "resolved {got}");
        let _ = std::fs::remove_dir_all(&target);
        let _ = std::fs::remove_dir_all(&decoy);
    }
}
