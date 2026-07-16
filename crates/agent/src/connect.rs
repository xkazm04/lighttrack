//! Result connectors — how a finished run propagates back into the originating app. `http` POSTs
//! the result envelope to the app's callback; `command` pipes it to a local script's stdin, which
//! covers any database or bespoke API without the agent needing drivers. Connectors receive the
//! task's idempotency key and must tolerate replays (relay delivery is at-least-once).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::actions::expand_headers;

/// Wall-clock cap on one `command` connector delivery. A wedged or runaway script settles the task
/// `failed` (a visible, retryable outcome) instead of hanging the serial agent loop forever.
const CONNECTOR_TIMEOUT: Duration = Duration::from_secs(60);
/// Upper bound on stderr we retain for the error message — a chatty script must not grow our RSS.
const MAX_STDERR: u64 = 8192;

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum ConnectorSpec {
    /// POST the result envelope as JSON. Header values may reference `${ENV_VAR}`s.
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    /// Run `command[0]` with `command[1..]` as args, envelope JSON on stdin, non-zero exit ⇒ error.
    Command { command: Vec<String> },
}

pub(crate) fn deliver(spec: &ConnectorSpec, envelope: &Value) -> Result<()> {
    match spec {
        ConnectorSpec::Http { url, headers } => {
            let client = reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(60))
                .build()
                .context("building connector HTTP client")?;
            let mut req = client.post(url).json(envelope);
            for (k, v) in expand_headers(headers)? {
                req = req.header(&k, v);
            }
            let resp = req.send().with_context(|| format!("POST {url}"))?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().unwrap_or_default();
                bail!("connector POST {url} -> HTTP {}: {}", status.as_u16(), truncate(&body));
            }
            Ok(())
        }
        ConnectorSpec::Command { command } => {
            let Some((prog, args)) = command.split_first() else {
                bail!("connector command is empty");
            };
            let mut child = Command::new(prog)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .with_context(|| format!("spawning connector '{prog}'"))?;

            // Write the envelope on a separate thread while we drain stderr and wait. Doing
            // `write_all`-then-`wait_with_output` inline deadlocks once either pipe fills its ~64KB
            // OS buffer: a child that emits to stderr while we're mid-write, or one that defers
            // reading stdin, wedges us — and the serial agent loop — forever. A BrokenPipe from the
            // writer is benign when the child exits cleanly (it simply didn't consume all of stdin).
            let mut stdin = child.stdin.take().context("connector stdin unavailable")?;
            let bytes = serde_json::to_vec(envelope)?;
            let writer = std::thread::spawn(move || stdin.write_all(&bytes));

            let mut stderr = child.stderr.take().context("connector stderr unavailable")?;
            let reader = std::thread::spawn(move || {
                let mut buf = Vec::new();
                // Keep only the first MAX_STDERR bytes, but keep draining to /dev/null so the child's
                // later stderr writes never block on a full pipe.
                let _ = stderr.by_ref().take(MAX_STDERR).read_to_end(&mut buf);
                let _ = std::io::copy(&mut stderr, &mut std::io::sink());
                buf
            });

            // Bound the wait: poll for exit against a deadline, then kill a runaway.
            let deadline = Instant::now() + CONNECTOR_TIMEOUT;
            let status = loop {
                match child.try_wait().context("waiting for connector")? {
                    Some(status) => break status,
                    None if Instant::now() >= deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = writer.join();
                        let _ = reader.join();
                        bail!(
                            "connector '{prog}' timed out after {}s",
                            CONNECTOR_TIMEOUT.as_secs()
                        );
                    }
                    None => std::thread::sleep(Duration::from_millis(20)),
                }
            };

            let write_res = writer.join().map_err(|_| anyhow!("connector stdin writer panicked"))?;
            let stderr_buf = reader.join().unwrap_or_default();

            if !status.success() {
                bail!(
                    "connector '{prog}' exited with {}: {}",
                    status.code().unwrap_or(-1),
                    truncate(String::from_utf8_lossy(&stderr_buf).trim())
                );
            }
            // Child succeeded: a BrokenPipe means it didn't read all of stdin, which is fine; any
            // other write error is a real delivery failure.
            if let Err(e) = write_res {
                if e.kind() != std::io::ErrorKind::BrokenPipe {
                    return Err(anyhow::Error::new(e).context("writing envelope to connector stdin"));
                }
            }
            Ok(())
        }
    }
}

/// Keep connector errors readable when they end up in the task's `error` column.
fn truncate(s: &str) -> String {
    const MAX: usize = 500;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn command_connector_pipes_stdin_and_reports_failure() {
        // `cmd /c findstr x` exits 1 when stdin lacks a match; `cmd /c more` always succeeds.
        #[cfg(windows)]
        {
            let ok = ConnectorSpec::Command { command: vec!["cmd".into(), "/c".into(), "findstr".into(), "task".into()] };
            deliver(&ok, &json!({ "task_id": "t-1" })).unwrap();
            let fail = ConnectorSpec::Command { command: vec!["cmd".into(), "/c".into(), "findstr".into(), "zzz_no_match".into()] };
            assert!(deliver(&fail, &json!({ "task_id": "t-1" })).is_err());
        }
        #[cfg(not(windows))]
        {
            let ok = ConnectorSpec::Command { command: vec!["grep".into(), "task".into()] };
            deliver(&ok, &json!({ "task_id": "t-1" })).unwrap();
            let fail = ConnectorSpec::Command { command: vec!["grep".into(), "zzz_no_match".into()] };
            assert!(deliver(&fail, &json!({ "task_id": "t-1" })).is_err());
        }
        let empty = ConnectorSpec::Command { command: vec![] };
        assert!(deliver(&empty, &json!({})).is_err());
    }

    #[test]
    fn command_connector_handles_envelope_larger_than_pipe_buffer() {
        // ~100KB envelope — well past the 64KB OS pipe buffer that made `write_all`-then-`wait`
        // deadlock (or, for a non-reading child, spuriously error on BrokenPipe). "task" is present
        // so the matcher exits 0; delivery must succeed.
        let big = json!({ "task_id": "task", "blob": "x".repeat(100_000) });
        #[cfg(windows)]
        let ok = ConnectorSpec::Command {
            command: vec!["cmd".into(), "/c".into(), "findstr".into(), "task".into()],
        };
        #[cfg(not(windows))]
        let ok = ConnectorSpec::Command { command: vec!["grep".into(), "task".into()] };
        deliver(&ok, &big).unwrap();
    }
}
