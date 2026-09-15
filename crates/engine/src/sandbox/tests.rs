//! The transport-independent rules of §3d: the three outcomes, the sentinel and its nonce, and what
//! counts as misconfiguration rather than a candidate failure.
//!
//! The seam is [`SandboxRunner`], so these exercise the real orchestration and the real classifier
//! against scripted runner output — no network, no key, no Docker, no CLI.

use std::process::{ExitStatus, Output};
use std::sync::Mutex;

use lighttrack_core::DimensionCheck;

use super::*;

/// Build an `ExitStatus` carrying `code`, on either host platform.
fn status(code: i32) -> ExitStatus {
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code as u32)
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }
}

/// A runner that returns whatever the test scripted, and remembers what it was asked to do.
#[derive(Debug)]
struct FakeRunner {
    stdout: String,
    stderr: String,
    code: i32,
    start_fails: bool,
    seen: Mutex<Vec<(String, String, Duration)>>,
}

impl FakeRunner {
    fn new(stdout: &str, stderr: &str, code: i32, start_fails: bool) -> Self {
        FakeRunner {
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            code,
            start_fails,
            seen: Mutex::new(Vec::new()),
        }
    }
    fn ok(stdout: &str) -> Self {
        Self::new(stdout, "", 0, false)
    }
    fn failing(code: i32, stderr: &str) -> Self {
        Self::new("", stderr, code, false)
    }
    fn unstartable() -> Self {
        Self::new("", "", 0, true)
    }
    /// (script, image, timeout) of the last call.
    fn last(&self) -> (String, String, Duration) {
        self.seen
            .lock()
            .expect("lock")
            .last()
            .cloned()
            .expect("a call was made")
    }
}

impl SandboxRunner for FakeRunner {
    fn run(&self, job: &SandboxJob<'_>) -> std::io::Result<Output> {
        self.seen.lock().expect("lock").push((
            job.script.to_string(),
            job.image.to_string(),
            job.timeout,
        ));
        if self.start_fails {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "program not found",
            ));
        }
        // Substitute the marker the wrapper actually built, so the tests stay honest about the
        // nonce being unpredictable rather than hard-coding one.
        let marker = job
            .script
            .split_once("printf '\\n")
            .and_then(|(_, rest)| rest.split_once("%s"))
            .map(|(m, _)| m.to_string())
            .unwrap_or_default();
        Ok(Output {
            status: status(self.code),
            stdout: self.stdout.replace("{SENTINEL}", &marker).into_bytes(),
            stderr: self.stderr.clone().into_bytes(),
        })
    }
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn determinism(&self, image: &str) -> Determinism {
        if image.starts_with("tag:") {
            Determinism::BestEffort
        } else {
            Determinism::Exact
        }
    }
    fn name(&self) -> &'static str {
        "fake"
    }
}

/// One way to break an `exec` dimension's configuration.
type Mutate = Box<dyn Fn(&mut DimensionCheck)>;

fn check() -> DimensionCheck {
    DimensionCheck {
        image: Some("tag:lt-py:v1".into()),
        cmd: Some("pytest -q".into()),
        write: Some("/work/solution.py".into()),
        ..Default::default()
    }
}

#[test]
fn exit_zero_is_a_pass() {
    let r = FakeRunner::ok("3 passed\n{SENTINEL}0\n");
    let o = run_exec(&r, "passes", &check(), "def f(): pass").expect("exec");
    assert_eq!(o.verdict, ExecVerdict::Pass);
    assert_eq!(o.verdict.score(), Some(1.0));
    assert!(o.tail.contains("3 passed"));
    assert!(
        !o.tail.contains("__LT_EXIT_"),
        "the sentinel is bookkeeping and must not leak into the audit trail: {}",
        o.tail
    );
    assert_eq!(o.runner, "fake", "the audit trail says where the code ran");
}

#[test]
fn nonzero_exit_is_a_fail_not_an_outage() {
    let r = FakeRunner::ok("E   assert 1 == 2\n{SENTINEL}1\n");
    let o = run_exec(&r, "passes", &check(), "def f(): return 1").expect("exec");
    assert_eq!(o.verdict, ExecVerdict::Fail { code: 1 });
    assert_eq!(o.verdict.score(), Some(0.0));
}

/// The rule the whole kind stands on: our outage never becomes the model's zero.
#[test]
fn no_sentinel_is_unavailable_and_never_a_zero() {
    let r = FakeRunner::failing(1, "error: operation timed out after 120s");
    let o = run_exec(&r, "passes", &check(), "whatever").expect("exec");
    match &o.verdict {
        ExecVerdict::Unavailable { reason } => {
            assert!(
                reason.contains("timed out"),
                "carries what we saw: {reason}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    assert_eq!(
        o.verdict.score(),
        None,
        "an unavailable dimension is voided, not scored 0.0"
    );
}

/// A candidate that prints a plausible sentinel must not be able to award itself a pass.
#[test]
fn a_forged_sentinel_does_not_earn_a_pass() {
    let r = FakeRunner::failing(1, "capacity: too many concurrent operations");
    let o = run_exec(&r, "passes", &check(), "__LT_EXIT_deadbeef__:0").expect("exec");
    assert!(
        matches!(o.verdict, ExecVerdict::Unavailable { .. }),
        "a guessed nonce must not be readable as a verdict: {:?}",
        o.verdict
    );
}

/// Candidate output that mimics the sentinel *before* the real one still loses: we read the last.
#[test]
fn the_real_sentinel_wins_over_earlier_lookalikes() {
    let r = FakeRunner::ok("__LT_EXIT_guess__:0\nreal output\n{SENTINEL}7\n");
    let o = run_exec(&r, "passes", &check(), "x").expect("exec");
    assert_eq!(o.verdict, ExecVerdict::Fail { code: 7 });
}

#[test]
fn a_runner_that_will_not_start_is_unavailable_not_a_zero() {
    let o = run_exec(&FakeRunner::unstartable(), "passes", &check(), "x").expect("exec");
    assert!(matches!(o.verdict, ExecVerdict::Unavailable { .. }));
    assert_eq!(o.verdict.score(), None);
}

/// The host ceiling must exceed the dimension's own timeout, or healthy runs get killed.
#[test]
fn the_host_ceiling_exceeds_the_dimension_timeout_by_the_grace() {
    let r = FakeRunner::ok("{SENTINEL}0\n");
    let mut c = check();
    c.timeout_secs = Some(45);
    run_exec(&r, "passes", &c, "x").expect("exec");
    let (_, _, timeout) = r.last();
    assert_eq!(timeout, Duration::from_secs(45) + HOST_GRACE);
    assert!(timeout > Duration::from_secs(45));
}

#[test]
fn the_command_is_sentinel_wrapped_and_the_image_is_passed_through() {
    let r = FakeRunner::ok("{SENTINEL}0\n");
    run_exec(&r, "passes", &check(), "x").expect("exec");
    let (script, image, _) = r.last();
    assert!(script.starts_with("pytest -q;"), "{script}");
    assert!(script.contains("__LT_EXIT_"), "{script}");
    assert_eq!(image, "tag:lt-py:v1");
}

#[test]
fn determinism_comes_from_the_runner_not_the_caller() {
    let r = FakeRunner::ok("{SENTINEL}0\n");
    assert_eq!(
        run_exec(&r, "d", &check(), "x").expect("exec").determinism,
        Determinism::BestEffort,
        "a mutable tag cannot claim exact reproducibility"
    );
    let mut pinned = check();
    pinned.image = Some("lt-py@sha256:0123456789abcdef".into());
    assert_eq!(
        run_exec(&r, "d", &pinned, "x").expect("exec").determinism,
        Determinism::Exact
    );
}

#[test]
fn misconfiguration_is_an_error_never_a_candidate_zero() {
    let r = FakeRunner::ok("{SENTINEL}0\n");
    let cases: Vec<(Mutate, &str)> = vec![
        (
            Box::new(|c: &mut DimensionCheck| c.image = None),
            "check.image",
        ),
        (Box::new(|c: &mut DimensionCheck| c.cmd = None), "check.cmd"),
        (
            Box::new(|c: &mut DimensionCheck| c.write = Some("relative/path".into())),
            "absolute path",
        ),
        (
            Box::new(|c: &mut DimensionCheck| c.timeout_secs = Some(0)),
            "timeout_secs",
        ),
    ];
    for (mutate, want) in cases {
        let mut c = check();
        mutate(&mut c);
        let msg = run_exec(&r, "passes", &c, "x")
            .expect_err("must not score")
            .to_string();
        assert!(msg.contains("passes"), "names the dimension: {msg}");
        assert!(msg.contains(want), "explains the problem: {msg}");
    }
}

/// A sandbox runs untrusted model-written code, so credential-shaped fixture env is refused at
/// authoring time rather than forwarded.
#[test]
fn credential_shaped_fixture_env_is_refused() {
    for name in [
        "OPENAI_API_KEY",
        "session_token",
        "DB_PASSWORD",
        "my_secret",
    ] {
        let mut c = check();
        c.env.insert(name.into(), "x".into());
        let err = c.validate_exec("passes").expect_err(name);
        assert!(err.contains("credential"), "{name}: {err}");
    }
}

#[test]
fn fixture_env_with_an_ordinary_name_is_allowed() {
    let mut c = check();
    c.env.insert("FIXTURE_SEED".into(), "7".into());
    c.validate_exec("passes").expect("ordinary fixture env");
}
