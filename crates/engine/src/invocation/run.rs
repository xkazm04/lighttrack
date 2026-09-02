//! The one spawn site: build the posture-approved command, feed the prompt over stdin, reap the
//! child against a wall clock, parse the envelope.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::posture::{apply_auth, plan};
use super::{completion_text, envelope, model_of, token_counts, ClaudeBin, Invocation, RawOutcome};
use crate::{EngineError, Result};

/// Run one headless Claude call.
///
/// The prompt travels over **stdin**, never argv: a judge prompt routinely runs to tens of
/// kilobytes, Windows caps a command line at ~32k characters, and every layer between here and the
/// child gets a chance to mangle embedded quotes. An empty prompt is the one exception — there
/// stdin is closed (`Stdio::null()`) so the child cannot block waiting for input that never comes.
pub fn run(cfg: &ClaudeBin, inv: &Invocation<'_>) -> Result<RawOutcome> {
    let plan = plan(inv)?;

    let mut cmd = Command::new(&cfg.bin);
    cmd.args(&plan.args)
        .current_dir(&plan.cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if inv.prompt.is_empty() {
        cmd.stdin(Stdio::null());
    } else {
        cmd.stdin(Stdio::piped());
    }
    apply_auth(&mut cmd, inv.bare)?;

    let started = Instant::now();
    let (status, stdout, stderr) = spawn_bounded(cmd, inv.prompt, inv.timeout, &cfg.bin)?;
    let latency_ms = Some(started.elapsed().as_millis() as u64);
    let stderr = String::from_utf8_lossy(&stderr).trim().to_string();
    let exit_ok = status.success();

    let raw: Value = match serde_json::from_slice(&stdout) {
        Ok(v) => v,
        // No envelope at all: for a plain completion a non-zero exit *is* the error (the judge's
        // retry classifier reads this stderr); with no envelope and a clean exit, the output is
        // simply unparseable.
        Err(e) if !exit_ok => {
            return Err(EngineError::NonZero {
                code: status.code().unwrap_or(-1),
                stderr: if stderr.is_empty() {
                    format!("no JSON envelope ({e})")
                } else {
                    stderr
                },
            })
        }
        Err(e) => {
            return Err(EngineError::Parse(format!(
                "envelope not JSON: {e}; stdout was: {}",
                String::from_utf8_lossy(&stdout)
            )))
        }
    };

    let is_error = raw
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(!exit_ok);
    // A `Generate` run has no budget cap and no tools, so a non-zero exit is a genuine failure and
    // is reported as one — unchanged from before the seam. The agentic modes pass
    // `--max-budget-usd`, whose enforcement exits non-zero but still prints a usable envelope, so
    // there the envelope wins and the caller decides what a capped run means.
    if plan.strict_exit && !exit_ok {
        return Err(EngineError::NonZero {
            code: status.code().unwrap_or(-1),
            stderr,
        });
    }

    let (input_tokens, output_tokens) = token_counts(&raw);
    Ok(RawOutcome {
        text: completion_text(&raw),
        json: envelope::structured(&raw).cloned(),
        model: model_of(&raw, inv.model),
        cost_usd: raw.get("total_cost_usd").and_then(Value::as_f64),
        input_tokens,
        output_tokens,
        latency_ms,
        subtype: raw
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        raw,
        exit_ok,
        is_error,
        stderr: if exit_ok && !is_error {
            String::new()
        } else {
            stderr
        },
    })
}

/// Spawn `cmd`, write `prompt` to its stdin, drain stdout/stderr on separate threads, and wait for
/// exit against a wall-clock `timeout`. On expiry the child is killed and reaped and a retryable
/// [`EngineError::Timeout`] is returned.
///
/// All three pipes are serviced on their own threads. That is load-bearing in both directions:
/// reading the output pipes only after the wait deadlocks the instant either fills its ~64KB OS
/// buffer, and writing a large prompt inline deadlocks symmetrically once the child's stdin buffer
/// fills while it is busy writing output nobody is reading yet.
fn spawn_bounded(
    mut cmd: Command,
    prompt: &str,
    timeout: Duration,
    bin: &str,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    let mut child = cmd.spawn().map_err(|source| EngineError::Spawn {
        bin: bin.to_string(),
        source,
    })?;
    let stdin_writer = child.stdin.take().map(|mut pipe| {
        let body = prompt.to_string();
        std::thread::spawn(move || {
            // A child that exits before reading the whole prompt (bad flag, instant refusal) breaks
            // the pipe; that is the child's exit status to report, not an error of its own.
            let _ = pipe.write_all(body.as_bytes());
            drop(pipe); // EOF — otherwise `claude -p` waits for more prompt forever
        })
    });
    let mut out_pipe = child.stdout.take().ok_or_else(|| {
        EngineError::Other("claude child was spawned without a stdout pipe".to_string())
    })?;
    let mut err_pipe = child.stderr.take().ok_or_else(|| {
        EngineError::Other("claude child was spawned without a stderr pipe".to_string())
    })?;
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().map_err(|source| EngineError::Spawn {
            bin: bin.to_string(),
            source,
        })? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_reader.join();
                let _ = err_reader.join();
                if let Some(w) = stdin_writer {
                    let _ = w.join();
                }
                return Err(EngineError::Timeout {
                    who: format!("claude -p (>{}s)", timeout.as_secs()),
                });
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    };
    if let Some(w) = stdin_writer {
        let _ = w.join();
    }
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    Ok((status, stdout, stderr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invocation::Mode;

    #[test]
    fn the_reaper_kills_a_child_that_outlives_the_timeout() {
        // A real long-running child that ignores stdin, so we exercise the actual kill+reap path,
        // not the claude arg shape. Bounded to 200ms; the child would otherwise run for seconds.
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("ping");
            c.args(["-n", "5", "127.0.0.1"]);
            c
        } else {
            let mut c = Command::new("sleep");
            c.arg("5");
            c
        };
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let res = spawn_bounded(cmd, "", Duration::from_millis(200), "sleeper");
        let elapsed = started.elapsed();
        match res {
            Err(EngineError::Timeout { who }) => assert!(who.contains("claude -p")),
            other => panic!("expected EngineError::Timeout, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(3),
            "spawn_bounded should return at the deadline, took {elapsed:?}"
        );
    }

    /// The stdin transport, end to end, against a stand-in binary: whatever we write to the child's
    /// stdin is what the child reads. No `claude` involved, so this runs anywhere.
    #[test]
    fn the_prompt_travels_over_stdin_and_reaches_the_child() {
        let prompt = "line one\n\"quoted\" & <angled>\n".repeat(500); // >10KB, quote-heavy
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("findstr");
            c.args(["/N", "^"]); // number every line it reads from stdin
            c
        } else {
            Command::new("cat")
        };
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (status, stdout, _) =
            spawn_bounded(cmd, &prompt, Duration::from_secs(30), "echoer").unwrap();
        assert!(status.success());
        let echoed = String::from_utf8_lossy(&stdout);
        assert!(echoed.contains("\"quoted\" & <angled>"), "quotes mangled");
        assert_eq!(
            echoed.matches("line one").count(),
            500,
            "the whole prompt should reach the child, not a truncated command line"
        );
    }

    /// A posture violation must never reach a spawn — even with a binary name that cannot exist.
    #[test]
    fn a_posture_violation_short_circuits_before_the_spawn() {
        let cfg = ClaudeBin::new("definitely-not-an-executable-lighttrack-test");
        let inv = Invocation::edit("x", "sonnet"); // no cwd, no permission mode
        assert_eq!(inv.mode, Mode::Edit);
        match run(&cfg, &inv) {
            Err(EngineError::Posture(msg)) => assert!(msg.contains("workspace"), "{msg}"),
            other => panic!("expected a posture error, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_binary_reports_a_spawn_error() {
        let cfg = ClaudeBin::new("definitely-not-an-executable-lighttrack-test");
        match run(&cfg, &Invocation::generate("hi", "haiku")) {
            Err(EngineError::Spawn { bin, .. }) => assert!(bin.contains("lighttrack-test")),
            other => panic!("expected a spawn error, got {other:?}"),
        }
    }
}
