//! The remote `exec` runner: a VM-isolated sandbox, via the `contree` CLI.
//!
//! Not the default — see [`super::docker`] for why a local container answers four of §3d's five
//! open questions better. This runner earns its place on the one axis Docker cannot match: VM-level
//! isolation, for the day a benchmark case's input is attacker-influenced rather than hand-authored.
//!
//! Blocked as of 2026-09-05: Sandboxes is a beta service that must be enabled per project, and a
//! token without it saves a profile that reports `status: "inactive"` and then answers every call
//! with `HTTP 403: Insufficient permissions: spawn and list`. [`ContreeCli::preflight`] turns that
//! into one named refusal at run start.

use std::process::Command;

use crate::{Determinism, EngineError, Result};

use super::{
    spawn_with_timeout, tail_str, SandboxJob, SandboxRunner, TempCandidate, OUTPUT_CAP_BYTES,
    PREFLIGHT_TIMEOUT, TAIL,
};

/// Hands each worker thread a small stable integer, so `-S <base>-<slot>` gives one session per
/// thread rather than one per case.
///
/// Per case would mint thousands of rows in the CLI's local session database for no benefit — every
/// run is `--disposable` with an explicit `--use`, so nothing about the session carries between
/// cases. Per *thread* is the granularity that actually matters, because that is the only level at
/// which two invocations are ever in flight against the same SQLite file at once.
fn thread_slot() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    thread_local! {
        static SLOT: usize = NEXT.fetch_add(1, Ordering::Relaxed);
    }
    SLOT.with(|s| *s)
}

/// Runs each case in a remote VM-isolated sandbox.
#[derive(Debug, Clone)]
pub struct ContreeCli {
    /// Binary name or absolute path. Not assumed present: a missing binary is a named, loud error.
    pub bin: String,
    /// `CONTREE_PROFILE` for this run, so benchmark credentials can be kept apart from an
    /// operator's own.
    pub profile: Option<String>,
    /// Session **base** name (`-S`); the per-thread slot is appended.
    pub session: Option<String>,
}

impl Default for ContreeCli {
    fn default() -> Self {
        ContreeCli {
            bin: "contree".to_string(),
            profile: None,
            session: None,
        }
    }
}

impl ContreeCli {
    fn base(&self) -> Command {
        let mut c = Command::new(&self.bin);
        if let Some(s) = &self.session {
            c.arg("-S").arg(format!("{s}-{}", thread_slot()));
        }
        if let Some(p) = &self.profile {
            c.env("CONTREE_PROFILE", p);
        }
        c
    }
}

impl SandboxRunner for ContreeCli {
    fn run(&self, job: &SandboxJob<'_>) -> std::io::Result<std::process::Output> {
        // Unlike Docker, `contree run` takes host files with `--file` rather than stdin (stdin is
        // forwarded to the command itself), so the candidate goes to disk for the length of the
        // case and is removed on drop however the case ends.
        let tmp = TempCandidate::write(job.candidate, "exec")
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut c = self.base();
        c.arg("run")
            .arg("--use")
            .arg(job.image)
            // Disposable: no state survives the case, so cases cannot contaminate each other and no
            // session branch has to be created or cleaned up.
            .arg("-D")
            .arg("-F")
            .arg(format!("{}:{}", tmp.path.display(), job.guest_path))
            .arg("-t")
            .arg(job.timeout.as_secs().to_string())
            .arg("-T")
            .arg(OUTPUT_CAP_BYTES.to_string());
        for (k, v) in job.env {
            c.arg("-e").arg(format!("{k}={v}"));
        }
        c.arg("-s").arg("--").arg(job.script);
        let out = spawn_with_timeout(c, None, job.timeout, "contree", || {});
        drop(tmp);
        out
    }

    /// Confirm the CLI is present and a profile is usable, once per run rather than once per case.
    ///
    /// This exists because of a specific trap: `contree auth` is the only subcommand that reads
    /// `NEBIUS_API_KEY` / `NEBIUS_AI_PROJECT` from the environment — `contree run` ignores them
    /// entirely and reads the saved profile. A keyed `.env` therefore looks configured and is not,
    /// and without this the failure would arrive as a 403 on every case, dutifully recorded as
    /// `unavailable` a thousand times over.
    fn preflight(&self) -> Result<()> {
        let mut c = self.base();
        c.arg("-o").arg("json").arg("auth").arg("ls");
        let out =
            spawn_with_timeout(c, None, PREFLIGHT_TIMEOUT, "contree", || {}).map_err(|e| {
                EngineError::Other(format!(
                "the rubric has an `exec` dimension but the `contree` CLI could not be started \
                 ({e}). Install it (`uv tool install contree-cli`) and run `contree auth`."
            ))
            })?;
        if !out.status.success() {
            return Err(EngineError::Other(format!(
                "the rubric has an `exec` dimension but `contree auth ls` failed: {}",
                tail_str(&String::from_utf8_lossy(&out.stderr), TAIL)
            )));
        }
        check_profiles(&String::from_utf8_lossy(&out.stdout))
    }

    fn determinism(&self, image: &str) -> Determinism {
        if image.starts_with("tag:") {
            Determinism::BestEffort
        } else {
            Determinism::Exact
        }
    }

    fn name(&self) -> &'static str {
        "contree"
    }
}

/// Split out so the profile rules are testable without a binary.
fn check_profiles(stdout: &str) -> Result<()> {
    // The CLI writes a human `[INFO]` banner to stdout and then one JSON object per profile, so the
    // object lines are read and the rest ignored rather than parsing the whole stream.
    let profiles: Vec<serde_json::Value> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if profiles.is_empty() {
        return Err(EngineError::Other(
            "the rubric has an `exec` dimension but no `contree` profile is configured. Set \
             NEBIUS_API_KEY and NEBIUS_AI_PROJECT, then run `contree auth -y`."
                .to_string(),
        ));
    }
    // A saved profile is not a working one.
    let active = profiles
        .iter()
        .find(|p| p.get("active").and_then(serde_json::Value::as_bool) == Some(true))
        .unwrap_or(&profiles[0]);
    if active.get("status").and_then(serde_json::Value::as_str) == Some("inactive") {
        return Err(EngineError::Other(format!(
            "the rubric has an `exec` dimension but the active `contree` profile is inactive: the \
             token is valid and Sandboxes is not enabled on project '{}'. Enable it for that \
             project (or point NEBIUS_AI_PROJECT at one where it is) and re-run `contree auth -y`.",
            active
                .get("project")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("(unnamed)")
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inactive_profile_is_refused_and_the_project_is_named() {
        let e = check_profiles(
            "[INFO] Configured profiles (* stands for active)\n\
             {\"name\": \"default\", \"project\": \"default-project\", \"active\": true, \"status\": \"inactive\"}\n",
        )
        .expect_err("inactive")
        .to_string();
        assert!(e.contains("default-project"), "names the project: {e}");
        assert!(e.contains("not enabled"), "says what is wrong: {e}");
    }

    #[test]
    fn the_info_banner_is_not_mistaken_for_an_absent_profile() {
        check_profiles(
            "[INFO] Configured profiles (* stands for active)\n\
             {\"name\": \"default\", \"project\": \"p\", \"active\": true, \"status\": \"ok\"}\n",
        )
        .expect("an active profile is usable");
    }

    #[test]
    fn no_profile_at_all_says_how_to_make_one() {
        let e = check_profiles("").expect_err("none").to_string();
        assert!(e.contains("contree auth"), "{e}");
    }

    #[test]
    fn only_an_immutable_pin_claims_exact_reproducibility() {
        let c = ContreeCli::default();
        assert_eq!(c.determinism("tag:lt-py:v1"), Determinism::BestEffort);
        assert_eq!(
            c.determinism("0f5a1b2c-3d4e-5f60-7182-93a4b5c6d7e8"),
            Determinism::Exact
        );
    }
}
