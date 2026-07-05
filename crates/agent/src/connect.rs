//! Result connectors — how a finished run propagates back into the originating app. `http` POSTs
//! the result envelope to the app's callback; `command` pipes it to a local script's stdin, which
//! covers any database or bespoke API without the agent needing drivers. Connectors receive the
//! task's idempotency key and must tolerate replays (relay delivery is at-least-once).

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::actions::expand_headers;

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
            child
                .stdin
                .take()
                .context("connector stdin unavailable")?
                .write_all(serde_json::to_string(envelope)?.as_bytes())
                .context("writing envelope to connector stdin")?;
            let out = child.wait_with_output().context("waiting for connector")?;
            if !out.status.success() {
                bail!(
                    "connector '{prog}' exited with {}: {}",
                    out.status.code().unwrap_or(-1),
                    truncate(String::from_utf8_lossy(&out.stderr).trim())
                );
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
}
