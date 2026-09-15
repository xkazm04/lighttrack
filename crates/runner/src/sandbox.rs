//! Wiring the `exec` rubric dimension's sandbox into the runner (docs/BENCHMARK_FRAMEWORK.md §3d).
//!
//! The whole module is the translation from CLI flags to one [`SandboxRunner`]. Everything else
//! about `exec` lives in the engine, because the runner is not the only thing that judges.

use std::sync::Arc;

use clap::ValueEnum;
use lighttrack_engine::{ContreeCli, DockerCli, SandboxRunner};

use crate::cli::Cli;

/// Which isolation runner `exec` dimensions use.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum SandboxKind {
    /// A disposable local container. The default: no account, no beta gate, `--network=none`, and a
    /// digest pin that stamps verdicts `exact`.
    Docker,
    /// A remote VM-isolated sandbox. Stronger isolation, for when a case's input is not
    /// hand-authored; requires Sandboxes enabled on the Nebius project.
    Contree,
}

/// The sandbox this run will use, or `None` when `--sandbox` was not passed.
///
/// Opt-in rather than auto-detected on purpose. `exec` is the one dimension kind that executes
/// anything, and a run that silently acquired the ability because a binary happened to be on `PATH`
/// would be a surprise in exactly the place — a gate before a deploy — where surprises are least
/// welcome.
pub(crate) fn from_cli(cli: &Cli) -> Option<Arc<dyn SandboxRunner>> {
    match cli.sandbox? {
        SandboxKind::Docker => Some(Arc::new(DockerCli {
            bin: cli.docker_bin.clone(),
        })),
        SandboxKind::Contree => Some(Arc::new(ContreeCli {
            bin: cli.contree_bin.clone(),
            profile: cli.contree_profile.clone(),
            session: Some(session_base()),
        })),
    }
}

/// The contree session base name for this process; the engine appends a per-worker-thread slot.
///
/// Keyed by process id so two `lt-runner` invocations on one machine — an operator's ad-hoc `bench`
/// beside a `serve` worker, which is the normal case — never share a session and therefore never
/// contend on the CLI's per-profile SQLite database. Docker needs no equivalent: each case is its
/// own container, named by the case id.
fn session_base() -> String {
    format!("lt-runner-{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("cli")
    }

    #[test]
    fn the_sandbox_is_off_unless_asked_for() {
        let cli = parse(&["lt-runner", "score", "--rubric-id", "r1"]);
        assert!(
            from_cli(&cli).is_none(),
            "the ability to execute anything is never acquired implicitly"
        );
    }

    #[test]
    fn docker_is_the_runner_you_get_by_naming_it() {
        let cli = parse(&[
            "lt-runner",
            "--sandbox",
            "docker",
            "score",
            "--rubric-id",
            "r1",
        ]);
        let r = from_cli(&cli).expect("configured");
        assert_eq!(r.name(), "docker");
    }

    #[test]
    fn contree_is_reachable_with_its_own_flags() {
        let cli = parse(&[
            "lt-runner",
            "--sandbox",
            "contree",
            "--contree-profile",
            "bench",
            "score",
            "--rubric-id",
            "r1",
        ]);
        let r = from_cli(&cli).expect("configured");
        assert_eq!(r.name(), "contree");
    }

    /// A digest pin is the only honest `exact`, and it is the runner that decides — not the rubric.
    #[test]
    fn the_chosen_runner_owns_the_determinism_rule() {
        let cli = parse(&["lt-runner", "--sandbox", "docker", "score"]);
        let r = from_cli(&cli).expect("configured");
        assert_eq!(
            r.determinism("lt-py@sha256:abc").as_str(),
            "exact",
            "a digest pins one image forever"
        );
        assert_eq!(r.determinism("lt-py:v1").as_str(), "best-effort");
    }

    #[test]
    fn neither_binary_is_assumed_to_be_on_path() {
        let cli = parse(&["lt-runner", "--sandbox", "docker", "score"]);
        assert_eq!(cli.docker_bin, "docker");
        assert_eq!(cli.contree_bin, "contree");
    }
}
