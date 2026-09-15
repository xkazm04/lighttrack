//! The default `exec` runner: a local container, one per case.
//!
//! Chosen over the remote sandbox as the default because on four of the five open questions
//! docs/BENCHMARK_FRAMEWORK.md §3d records, a local container is the stronger answer:
//!
//! | §3d open question | here |
//! | --- | --- |
//! | #1 price | no vendor price at all, so `--max-cost` keeps its meaning |
//! | #2 cold start | local, small, and measurable rather than an undocumented round trip |
//! | #3 egress | **closed** — `--network=none`, rather than "assume the code reaches the internet" |
//! | #5 data residency | dissolves; nothing leaves the machine |
//!
//! What it gives up is container isolation instead of VM isolation, and one machine's worth of
//! capacity. For code a model wrote against a task *we* declared, `--network=none` plus memory, CPU
//! and PID caps on a disposable container is proportionate. It stops being proportionate if a case's
//! input is attacker-influenced — which §1's "sample datasets from real production events" makes
//! possible — and that is the day [`super::ContreeCli`] earns its beta access.

use std::process::Command;

use crate::{Determinism, EngineError, Result};

use super::{spawn_with_timeout, tail_str, SandboxJob, SandboxRunner, PREFLIGHT_TIMEOUT, TAIL};

/// Memory ceiling per case. Generous for a test suite, far below anything that would disturb the
/// host — a runaway allocation in generated code is a normal failure mode, not an exotic one.
const MEMORY: &str = "512m";
/// CPU ceiling per case. One core keeps a hot loop from starving the judge calls running beside it.
const CPUS: &str = "1";
/// Process ceiling per case: a fork bomb is a plausible thing for generated code to be.
const PIDS: &str = "256";

/// Runs each case in a disposable local container.
#[derive(Debug, Clone)]
pub struct DockerCli {
    /// Binary name or absolute path. Not assumed present: a missing binary is a named, loud error.
    pub bin: String,
}

impl Default for DockerCli {
    fn default() -> Self {
        DockerCli {
            bin: "docker".to_string(),
        }
    }
}

impl DockerCli {
    fn container_name(&self, id: &str) -> String {
        format!("lt-exec-{id}")
    }
}

impl SandboxRunner for DockerCli {
    fn run(&self, job: &SandboxJob<'_>) -> std::io::Result<std::process::Output> {
        let name = self.container_name(job.id);
        let mut c = Command::new(&self.bin);
        c.arg("run")
            .arg("--rm")
            // The candidate arrives on stdin rather than through a bind mount. That is not a
            // stylistic choice: this repository's primary host is Windows, where translating a host
            // path into a Linux container is a reliable source of breakage, and a mount would also
            // have to be read-only to stop the candidate rewriting itself. Piping needs neither.
            .arg("-i")
            // Closes §3d's open question #3 outright rather than documenting it as a risk.
            .arg("--network=none")
            .arg(format!("--memory={MEMORY}"))
            .arg(format!("--cpus={CPUS}"))
            .arg(format!("--pids-limit={PIDS}"))
            // Named so a timeout can remove the container; `--rm` alone does not help when the
            // client is killed, because the daemon keeps running what it was told to run.
            .arg("--name")
            .arg(&name);
        // Literal fixture values only. The host environment is never forwarded, so there is no path
        // by which a provider or admin key reaches model-written code.
        for (k, v) in job.env {
            c.arg("-e").arg(format!("{k}={v}"));
        }
        c.arg(job.image);
        c.arg("/bin/sh").arg("-c").arg(entry_script(job));

        let bin = self.bin.clone();
        spawn_with_timeout(c, Some(job.candidate), job.timeout, "docker", move || {
            // The client is dead; the container is not. Reap it, or a timing-out benchmark leaves
            // one stopped container per case behind.
            let _ = Command::new(&bin)
                .arg("rm")
                .arg("-f")
                .arg(&name)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        })
    }

    fn preflight(&self) -> Result<()> {
        let mut c = Command::new(&self.bin);
        c.arg("version").arg("--format").arg("{{.Server.Version}}");
        let out = spawn_with_timeout(c, None, PREFLIGHT_TIMEOUT, "docker", || {}).map_err(|e| {
            EngineError::Other(format!(
                "the rubric has an `exec` dimension but `{}` could not be started ({e}). Install \
                 Docker, or pass --contree-bin/--sandbox contree to use the remote runner.",
                self.bin
            ))
        })?;
        if !out.status.success() {
            // The overwhelmingly common case, and worth naming precisely: the CLI is installed and
            // the daemon is not running. "docker: command not found" and "cannot connect to the
            // daemon" are different problems with different fixes.
            return Err(EngineError::Other(format!(
                "the rubric has an `exec` dimension but the Docker daemon is not reachable — the \
                 CLI is installed and the engine is not running. Start Docker Desktop (or the \
                 daemon) and try again. Detail: {}",
                tail_str(&String::from_utf8_lossy(&out.stderr), TAIL)
            )));
        }
        Ok(())
    }

    fn determinism(&self, image: &str) -> Determinism {
        // A digest names one immutable image forever; a tag is a moving pointer. Only the first can
        // honestly claim a re-run reproduces.
        if image.contains("@sha256:") {
            Determinism::Exact
        } else {
            Determinism::BestEffort
        }
    }

    fn name(&self) -> &'static str {
        "docker"
    }
}

/// The shell the container runs: take the candidate from stdin, put it where the rubric said, then
/// run the sentinel-wrapped command.
///
/// `mkdir -p` because the rubric's `write` path may name a directory the image does not have, and a
/// missing directory should be a loud harness failure rather than a silent write to nowhere.
fn entry_script(job: &SandboxJob<'_>) -> String {
    let dir = job
        .guest_path
        .rsplit_once('/')
        .map(|(d, _)| d)
        .filter(|d| !d.is_empty())
        .unwrap_or("/");
    format!(
        "set -e; mkdir -p '{dir}'; cat > '{path}'; set +e; {script}",
        path = job.guest_path,
        script = job.script
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn job<'a>(env: &'a BTreeMap<String, String>, script: &'a str) -> SandboxJob<'a> {
        SandboxJob {
            id: "abc123",
            image: "lt-py:v1",
            guest_path: "/work/solution.py",
            candidate: "print(1)",
            script,
            env,
            timeout: Duration::from_secs(30),
        }
    }

    #[test]
    fn the_entry_script_places_the_candidate_then_runs_the_command() {
        let env = BTreeMap::new();
        let s = entry_script(&job(&env, "pytest -q; printf X"));
        assert!(s.contains("mkdir -p '/work'"), "{s}");
        assert!(s.contains("cat > '/work/solution.py'"), "{s}");
        assert!(
            s.find("cat >").unwrap() < s.find("pytest").unwrap(),
            "the candidate must be in place before the command runs: {s}"
        );
        assert!(
            s.contains("set +e"),
            "the command must be allowed to fail so the sentinel still prints: {s}"
        );
    }

    /// A `write` path at the filesystem root must not produce `mkdir -p ''`.
    #[test]
    fn a_root_level_write_path_does_not_produce_an_empty_mkdir() {
        let env = BTreeMap::new();
        let mut j = job(&env, "true");
        j.guest_path = "/solution.py";
        let s = entry_script(&j);
        assert!(s.contains("mkdir -p '/'"), "{s}");
        assert!(!s.contains("mkdir -p ''"), "{s}");
    }

    #[test]
    fn only_a_digest_pin_claims_exact_reproducibility() {
        let d = DockerCli::default();
        assert_eq!(d.determinism("lt-py:v1"), Determinism::BestEffort);
        assert_eq!(
            d.determinism("lt-py@sha256:0123456789abcdef"),
            Determinism::Exact
        );
    }

    #[test]
    fn the_runner_names_itself_for_the_audit_trail() {
        assert_eq!(DockerCli::default().name(), "docker");
    }

    /// Not a style preference: an unreachable daemon and a missing binary have different fixes, and
    /// the message has to say which one happened.
    #[test]
    fn preflight_distinguishes_a_missing_binary_from_a_dead_daemon() {
        let missing = DockerCli {
            bin: "definitely-not-a-real-binary-xyz".into(),
        };
        let e = missing.preflight().expect_err("no binary").to_string();
        assert!(e.contains("could not be started"), "{e}");
        assert!(!e.contains("not reachable"), "wrong diagnosis: {e}");
    }
}
