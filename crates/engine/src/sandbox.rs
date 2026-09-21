//! The `exec` rubric dimension's execution seam (docs/BENCHMARK_FRAMEWORK.md §3d).
//!
//! The candidate output is written into an isolated environment built from a pinned image, one
//! command runs there, and that command's **exit code** is the verdict. Nothing about the candidate
//! is trusted — not its content, and not its stdout.
//!
//! ## Three outcomes, not two
//!
//! The rule this module exists to enforce is that **an infrastructure failure is not a zero**. A
//! timeout, an unreachable daemon, an image that will not pull, or a capacity ceiling are facts
//! about *us*; scoring them `0.0` would silently attribute our own outage to the model under test.
//! So [`ExecVerdict`] has a third arm, [`ExecVerdict::Unavailable`], which voids the dimension for
//! that case rather than failing the candidate.
//!
//! ## How we know which of the three happened
//!
//! A runner propagates the sandboxed command's exit code as its own, which makes "the code ran and
//! returned 1" and "nothing could be run" the same observation from outside. We therefore do not
//! read the runner's exit code as the verdict. Instead the command is wrapped so that it prints its
//! own status on a **sentinel line**, and the wrapper always exits 0:
//!
//! ```text
//! <cmd>; printf '\n__LT_EXIT_<nonce>__:%s\n' "$?"
//! ```
//!
//! - Sentinel present → the harness demonstrably ran to completion. Its code is the verdict.
//! - Sentinel absent → nothing completed (timeout, capacity, auth, a dead image). `Unavailable`.
//!
//! The sentinel carries a **per-case random nonce** for one reason: without it, a candidate could
//! print `__LT_EXIT__:0` itself and score a pass it did not earn. Model-written code cannot guess a
//! nonce minted after it was generated, so the sentinel proves the wrapper wrote it — the evidence
//! is unforgeable rather than merely conventional.
//!
//! ## Two runners behind one seam
//!
//! [`SandboxRunner`] is the whole vendor surface. [`DockerCli`] runs cases in a local container and
//! is the default; [`ContreeCli`] runs them in a remote VM-isolated sandbox. Everything above this
//! line — the three outcomes, the sentinel, the verdict arithmetic — is transport-independent, and
//! that is deliberate: the isolation vendor is the least durable part of this design.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use lighttrack_core::DimensionCheck;

use crate::{Determinism, EngineError, Result};

mod contree;
mod docker;

pub use contree::ContreeCli;
pub use docker::DockerCli;

/// Wall-clock ceiling for the sandboxed command when a dimension names none.
pub const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 120;

/// Bytes of sandbox stdout/stderr kept for the audit trail.
pub const OUTPUT_CAP_BYTES: u64 = 65536;

/// Longest stdout/stderr tail echoed into a dimension's reasoning string.
pub(crate) const TAIL: usize = 400;

/// Slack added to a dimension's own timeout to get the **host-side** ceiling.
///
/// A runner's own `--timeout` (where it has one) bounds the sandboxed command, not the client
/// process on this machine. Measured 2026-09-05: `contree images` against a project without
/// Sandboxes enabled produced no output for over three minutes and had to be killed — no error, no
/// timeout, nothing to classify. `docker run` has no client-side timeout at all. Without a ceiling
/// here one misconfigured environment hangs a benchmark worker indefinitely, which is worse than any
/// wrong score. The grace covers image pull and container start, so a command genuinely running to
/// its own deadline is not cut off early.
pub(crate) const HOST_GRACE: Duration = Duration::from_secs(60);

/// Host-side ceiling for a one-off preflight probe. Short on purpose: it exists to fail fast.
pub(crate) const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

/// What the sandbox did with one case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecVerdict {
    /// The command ran and exited 0 → the dimension scores 1.0.
    Pass,
    /// The command ran and exited non-zero → the dimension scores 0.0. The code *ran*; it was wrong.
    Fail { code: i32 },
    /// The command could not be run to completion, so this case has **no verdict**. Voids the
    /// dimension; never a 0.0. `reason` is operator-facing and names what we actually observed.
    Unavailable { reason: String },
}

impl ExecVerdict {
    /// The score this verdict contributes, or `None` when the dimension is voided.
    pub fn score(&self) -> Option<f64> {
        match self {
            ExecVerdict::Pass => Some(1.0),
            ExecVerdict::Fail { .. } => Some(0.0),
            ExecVerdict::Unavailable { .. } => None,
        }
    }
}

/// One sandboxed evaluation: the verdict plus everything needed to re-run it by hand.
#[derive(Debug, Clone)]
pub struct ExecOutcome {
    pub verdict: ExecVerdict,
    /// The image as pinned by the rubric.
    pub image: String,
    /// Which runner produced this — the audit trail must say where the code actually ran.
    pub runner: &'static str,
    /// `Exact` when the image was pinned immutably (a digest, or a contree UUID), `BestEffort` when
    /// pinned by a mutable tag: a tag may resolve to a different machine tomorrow, and a
    /// reproducibility claim that ignores that is a false one.
    pub determinism: Determinism,
    /// The command as sent (before sentinel wrapping).
    pub cmd: String,
    /// Trailing sandbox output, for `detail.reasoning`.
    pub tail: String,
    /// Real wall clock for the whole round trip — *our* latency, not the model's.
    pub latency_ms: u64,
}

impl ExecOutcome {
    /// The audit line recorded as this dimension's reasoning.
    pub fn reasoning(&self) -> String {
        let head = match &self.verdict {
            ExecVerdict::Pass => "exec: exit 0 → pass".to_string(),
            ExecVerdict::Fail { code } => format!("exec: exit {code} → fail"),
            ExecVerdict::Unavailable { reason } => {
                format!("exec: unavailable ({reason}) → voided, not scored")
            }
        };
        format!(
            "{head} [{} image {} ({}), cmd `{}`, {}ms]{}",
            self.runner,
            self.image,
            self.determinism.as_str(),
            self.cmd,
            self.latency_ms,
            if self.tail.is_empty() {
                String::new()
            } else {
                format!("\n{}", self.tail)
            }
        )
    }
}

/// One case's worth of work, in terms every runner understands.
pub struct SandboxJob<'a> {
    /// Unique per case — used to name the container so a timeout can clean it up, and already
    /// embedded in `script`'s sentinel.
    pub id: &'a str,
    /// Image reference, in whatever form the runner pins.
    pub image: &'a str,
    /// Absolute path inside the sandbox where the candidate must land.
    pub guest_path: &'a str,
    /// The candidate output itself. How it gets in is the runner's business.
    pub candidate: &'a str,
    /// The sentinel-wrapped shell to run once the candidate is in place.
    pub script: &'a str,
    /// Literal fixture environment. Never sourced from this host's environment.
    pub env: &'a BTreeMap<String, String>,
    /// Host-side ceiling; the runner must not exceed it.
    pub timeout: Duration,
}

/// The whole vendor surface. Implemented by [`DockerCli`] and [`ContreeCli`].
pub trait SandboxRunner: std::fmt::Debug + Send + Sync {
    /// Run one case. `Err` covers not starting the process and being killed for exceeding the
    /// timeout; a runner that ran and failed returns `Ok` with a non-zero status, because that is a
    /// different fact. Every arm still reaches the caller as `Unavailable`, never as a zero.
    fn run(&self, job: &SandboxJob<'_>) -> std::io::Result<Output>;

    /// Confirm this runner can work at all, **once per run** rather than once per case.
    fn preflight(&self) -> Result<()>;

    /// Whether this image reference pins one immutable machine.
    fn determinism(&self, image: &str) -> Determinism;

    /// Short name for the audit trail.
    fn name(&self) -> &'static str;
}

/// Run one case's `exec` dimension.
///
/// `Err` is reserved for a rubric that cannot be evaluated at all (missing `image`/`cmd`/`write`) —
/// an operator bug, which must never masquerade as a candidate scoring 0. Everything else,
/// including every runner failure, comes back as an [`ExecOutcome`].
pub fn run_exec(
    runner: &dyn SandboxRunner,
    key: &str,
    check: &DimensionCheck,
    candidate: &str,
) -> Result<ExecOutcome> {
    check.validate_exec(key).map_err(EngineError::Other)?;
    // Safe: validate_exec proved all three are present and non-empty.
    let image = check
        .image
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_string();
    let cmd = check.cmd.as_deref().unwrap_or_default().trim().to_string();
    let guest_path = check
        .write
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_string();
    let timeout = check.timeout_secs.unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS);

    let id = lighttrack_core::new_id();
    let script = wrap(&cmd, &id);
    let job = SandboxJob {
        id: &id,
        image: &image,
        guest_path: &guest_path,
        candidate,
        script: &script,
        env: &check.env,
        timeout: Duration::from_secs(timeout) + HOST_GRACE,
    };

    let started = Instant::now();
    let result = runner.run(&job);
    let latency_ms = started.elapsed().as_millis() as u64;

    let (verdict, tail) = match result {
        // The runner could not be started, or ran past the host ceiling and was killed. Both are
        // ours, and neither is a candidate scoring 0.
        Err(e) => (
            ExecVerdict::Unavailable {
                reason: if e.kind() == std::io::ErrorKind::TimedOut {
                    e.to_string()
                } else {
                    format!("could not start `{}`: {e}", runner.name())
                },
            },
            String::new(),
        ),
        Ok(out) => classify(&out, &id),
    };

    Ok(ExecOutcome {
        verdict,
        determinism: runner.determinism(&image),
        runner: runner.name(),
        image,
        cmd,
        tail,
        latency_ms,
    })
}

/// Wrap the operator's command so it reports its own exit status on an unforgeable sentinel line,
/// and so the wrapper itself always exits 0 — see the module docs.
pub(crate) fn wrap(cmd: &str, nonce: &str) -> String {
    format!("{cmd}; printf '\\n{}%s\\n' \"$?\"", sentinel(nonce))
}

pub(crate) fn sentinel(nonce: &str) -> String {
    format!("__LT_EXIT_{nonce}__:")
}

/// Read the verdict out of a completed runner invocation.
fn classify(out: &Output, nonce: &str) -> (ExecVerdict, String) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let marker = sentinel(nonce);

    // The last occurrence, not the first: the command's own output precedes the wrapper's line, and
    // a candidate that echoes something marker-shaped cannot get in *after* it.
    if let Some(code) = stdout.rmatch_indices(&marker).next().and_then(|(i, _)| {
        stdout[i + marker.len()..]
            .lines()
            .next()
            .map(str::trim)
            .and_then(|s| s.parse::<i32>().ok())
    }) {
        let verdict = if code == 0 {
            ExecVerdict::Pass
        } else {
            ExecVerdict::Fail { code }
        };
        // Strip the sentinel line from the audit trail; it is our bookkeeping, not the harness's
        // output, and leaving it in would teach every reader the nonce scheme for no benefit.
        let visible: String = stdout
            .lines()
            .filter(|l| !l.contains(&marker))
            .collect::<Vec<_>>()
            .join("\n");
        return (verdict, tail_str(&visible, TAIL));
    }

    // No sentinel: nothing ran to completion. Say what we saw, and say nothing about the candidate.
    let status = out
        .status
        .code()
        .map(|c| format!("runner exited {c}"))
        .unwrap_or_else(|| "runner was terminated by a signal".to_string());
    let detail = if stderr.trim().is_empty() {
        tail_str(&stdout, TAIL)
    } else {
        tail_str(&stderr, TAIL)
    };
    let reason = if detail.is_empty() {
        format!("{status}; the command never reported an exit status")
    } else {
        format!("{status}: {detail}")
    };
    (ExecVerdict::Unavailable { reason }, String::new())
}

/// Spawn `cmd`, feed it `stdin_bytes`, and kill it after `timeout`.
///
/// Shared by both runners because the hang it defends against is not vendor-specific. `on_timeout`
/// runs before the error is returned, so a runner can clean up whatever it left behind.
pub(crate) fn spawn_with_timeout(
    mut cmd: Command,
    stdin_bytes: Option<&str>,
    timeout: Duration,
    who: &str,
    on_timeout: impl FnOnce(),
) -> std::io::Result<Output> {
    let mut child = cmd
        .stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Write stdin on its own thread: a candidate larger than the pipe buffer would block us here,
    // and that block would look exactly like the hang the timeout exists to end.
    if let Some(bytes) = stdin_bytes {
        let mut sink = child.stdin.take();
        let owned = bytes.to_string();
        std::thread::spawn(move || {
            if let Some(s) = sink.as_mut() {
                let _ = s.write_all(owned.as_bytes());
            }
            // Dropping the handle closes the pipe, which is what tells `cat` it is done.
        });
    }

    // Drain both pipes on their own threads, for the same reason.
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let out_t = std::thread::spawn(move || read_all(&mut out));
    let err_t = std::thread::spawn(move || read_all(&mut err));

    let deadline = Instant::now() + timeout;
    let (status, timed_out) = loop {
        match child.try_wait()? {
            Some(s) => break (s, false),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                break (child.wait()?, true);
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };

    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    if timed_out {
        on_timeout();
        let last = tail_str(&String::from_utf8_lossy(&stderr), TAIL);
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "`{who}` did not return within {}s and was killed{}",
                timeout.as_secs(),
                if last.is_empty() {
                    String::new()
                } else {
                    format!("; last stderr: {last}")
                }
            ),
        ));
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn read_all(r: &mut Option<impl std::io::Read>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(r) = r.as_mut() {
        let _ = std::io::Read::read_to_end(r, &mut buf);
    }
    buf
}

/// The last `n` characters, prefixed with an ellipsis when something was dropped. The *tail* rather
/// than the head: a compiler's last lines say why it failed; its first say what it started.
pub(crate) fn tail_str(s: &str, n: usize) -> String {
    let s = s.trim();
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    std::iter::once('…')
        .chain(s.chars().skip(count - n))
        .collect()
}

/// The candidate output on this host's disk, removed when the case ends however it ends. Used by
/// runners that mount a file rather than reading stdin.
pub(crate) struct TempCandidate {
    pub(crate) path: PathBuf,
}

impl TempCandidate {
    pub(crate) fn write(candidate: &str, key: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("lt-exec-{}", lighttrack_core::new_id()));
        std::fs::write(&path, candidate.as_bytes()).map_err(|e| {
            EngineError::Other(format!(
                "rubric dimension '{key}' (exec): could not write the candidate to {}: {e}",
                path.display()
            ))
        })?;
        Ok(TempCandidate { path })
    }
}

impl Drop for TempCandidate {
    fn drop(&mut self) {
        // Best effort: a leaked temp file is not worth failing a scored case over.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests;
