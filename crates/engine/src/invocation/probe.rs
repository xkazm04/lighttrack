//! Is the CLI actually there, and plausibly authenticated — asked *before* a service claims paid
//! work.
//!
//! A responder or a `serve` worker with no `claude` on the box leases a task, spends the attempt,
//! and settles it failed; a fleet of them turns a missing install into a dead-lettered queue. The
//! probe is deliberately cheap and offline: `--version` costs nothing and spends nothing, and auth
//! is inferred from credentials that already exist rather than by making a call to find out.

use std::process::{Command, Stdio};
use std::time::Duration;

use super::resolve_claude_bin;

/// What a start-up probe learned about the local CLI.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    /// The binary exists and answered `--version`.
    pub installed: bool,
    /// The version string it reported, trimmed.
    pub version: Option<String>,
    /// `Some(true)` when a credential we recognise is present; `None` when we cannot tell without
    /// spending money, which is not a question a probe is allowed to answer.
    pub authed: Option<bool>,
    /// Why the probe failed, when it did — for the log line the caller prints.
    pub error: Option<String>,
}

impl Probe {
    /// A one-line summary for a start-up banner.
    pub fn summary(&self) -> String {
        if !self.installed {
            return format!(
                "claude NOT available ({})",
                self.error.as_deref().unwrap_or("unknown reason")
            );
        }
        let auth = match self.authed {
            Some(true) => "credentials found",
            Some(false) => "no credentials",
            None => "auth unknown",
        };
        format!(
            "claude {} ({auth})",
            self.version.as_deref().unwrap_or("(no version)")
        )
    }
}

/// Probe the CLI at `bin` (resolved through [`resolve_claude_bin`] when it is the bare name).
pub fn probe(bin: &str) -> Probe {
    let resolved = resolve_claude_bin(bin);
    let mut cmd = Command::new(&resolved);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match cmd.spawn() {
        Err(e) => Probe {
            error: Some(format!("spawning '{resolved}': {e}")),
            ..Probe::default()
        },
        Ok(child) => match wait_briefly(child) {
            None => Probe {
                error: Some(format!("'{resolved} --version' did not answer in time")),
                ..Probe::default()
            },
            Some((ok, out)) if ok => Probe {
                installed: true,
                version: Some(out).filter(|s| !s.is_empty()),
                authed: credentials_present(),
                error: None,
            },
            Some((_, out)) => Probe {
                error: Some(format!("'{resolved} --version' failed: {out}")),
                ..Probe::default()
            },
        },
    }
}

/// `--version` is instantaneous; anything slower is a broken install, not a slow one.
fn wait_briefly(mut child: std::process::Child) -> Option<(bool, String)> {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Some((false, e.to_string())),
        }
    }
    let out = child.wait_with_output().ok()?;
    let text = if out.stdout.is_empty() {
        String::from_utf8_lossy(&out.stderr).trim().to_string()
    } else {
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    Some((out.status.success(), text))
}

/// Whether *some* credential the CLI could use exists. `None` rather than `Some(false)` when we
/// simply don't know: a seat login can live in places we don't enumerate, and claiming "not
/// authenticated" would be a worse lie than admitting ignorance.
fn credentials_present() -> Option<bool> {
    if std::env::var_os("ANTHROPIC_API_KEY").is_some_and(|v| !v.is_empty()) {
        return Some(true);
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let creds = std::path::Path::new(&home)
        .join(".claude")
        .join(".credentials.json");
    if creds.exists() {
        return Some(true);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_binary_probes_as_not_installed() {
        let p = probe("definitely-not-an-executable-lighttrack-test");
        assert!(!p.installed);
        assert!(p.version.is_none());
        assert!(p.error.is_some());
        assert!(p.summary().contains("NOT available"));
    }

    /// A stand-in binary that answers `--version` proves the success path without a real claude.
    /// `git` is on PATH wherever this workspace is built.
    #[test]
    fn a_binary_that_answers_version_probes_as_installed() {
        let p = probe("git");
        assert!(p.installed, "probe of 'git' failed: {:?}", p.error);
        assert!(p.version.is_some());
        assert!(p.summary().starts_with("claude "));
    }

    /// Never a confident "not authenticated" — a seat login can live where we don't look, and
    /// claiming absence would be a worse lie than admitting ignorance.
    #[test]
    fn credentials_are_never_reported_as_a_confident_absence() {
        assert!(matches!(credentials_present(), Some(true) | None));
    }
}
