//! Proving a `codex exec` answer came from reasoning, not from a tool.
//!
//! `codex exec --json` does not report tool calls. The only record is the turn's session log
//! (`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<time>-<thread>.jsonl`), so after every call the log is
//! read, audited, and deleted. Disabling tools in [`super::posture`] is the first layer; this is the
//! second, and the one that holds when a Codex release adds a tool the first layer does not name.
//!
//! **Fail closed.** A call whose log cannot be found, or which ran any tool, is an error rather than a
//! scored answer. A tool the model *attempted* and Codex *refused* is fine — the answer still came
//! from reasoning — and is only logged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Once;

use serde_json::Value;

use crate::{EngineError, Result};

const WHO: &str = "codex exec";

/// Calls the model makes to a local tool; each is answered by a matching `*_output` item.
const LOCAL_CALLS: &[&str] = &["custom_tool_call", "function_call", "local_shell_call"];
const LOCAL_OUTPUTS: &[&str] = &[
    "custom_tool_call_output",
    "function_call_output",
    "local_shell_call_output",
];
/// Calls that run on OpenAI's side. They have no local output item, so only a `failed` status says
/// they did not run.
const HOSTED_CALLS: &[&str] = &[
    "web_search_call",
    "image_generation_call",
    "computer_call",
    "file_search_call",
    "code_interpreter_call",
    "tool_search_call",
    "mcp_call",
];

/// What a turn's log shows the model doing with tools.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ToolAudit {
    /// Tools the model asked for and Codex refused. The answer still came from reasoning.
    pub(super) refused: Vec<String>,
    /// Tools that ran. An answer with anything here is not a reasoning result.
    pub(super) executed: Vec<String>,
}

struct Call {
    label: String,
    call_id: Option<String>,
    hosted: bool,
    status: Option<String>,
}

/// Audit a whole session log.
pub(super) fn audit(log: &str) -> ToolAudit {
    let mut calls: Vec<Call> = Vec::new();
    let mut outputs: HashMap<String, String> = HashMap::new();
    for line in log.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let p = &v["payload"];
        let kind = p["type"].as_str().unwrap_or("");
        let hosted = HOSTED_CALLS.contains(&kind);
        if hosted || LOCAL_CALLS.contains(&kind) {
            calls.push(Call {
                label: p["name"].as_str().unwrap_or(kind).to_string(),
                call_id: p["call_id"].as_str().map(str::to_string),
                hosted,
                status: p["status"].as_str().map(str::to_string),
            });
        } else if LOCAL_OUTPUTS.contains(&kind) {
            if let Some(id) = p["call_id"].as_str() {
                outputs.insert(id.to_string(), output_text(&p["output"]));
            }
        }
    }
    let mut out = ToolAudit::default();
    for c in calls {
        let refused = if c.hosted {
            c.status.as_deref() == Some("failed")
        } else {
            // A local call with no output at all is not assumed refused: fail closed.
            c.call_id
                .as_ref()
                .and_then(|id| outputs.get(id))
                .is_some_and(|o| is_refusal(o))
        };
        if refused {
            out.refused.push(c.label);
        } else {
            out.executed.push(c.label);
        }
    }
    out
}

/// A tool output is a string, or a list of `{type, text}` parts.
fn output_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

/// Narrow on purpose: anything that does not read as Codex refusing the tool counts as a tool that
/// ran. A false "executed" costs one errored case; a false "refused" publishes a scripted answer as
/// reasoning.
fn is_refusal(output: &str) -> bool {
    let o = output.to_ascii_lowercase();
    [
        "is disabled",
        "not enabled",
        "unsupported call",
        "unknown tool",
    ]
    .iter()
    .any(|m| o.contains(m))
}

/// Where Codex writes session logs: `$CODEX_HOME/sessions`, else `~/.codex/sessions`.
fn sessions_root() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(home).join("sessions"));
    }
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|v| !v.is_empty())
        .map(|h| PathBuf::from(h).join(".codex").join("sessions"))
}

fn child_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// The session log for `thread_id`, searched in the three newest day directories — a call that
/// straddles midnight writes under either day. Zero-padded `YYYY/MM/DD` names sort chronologically.
fn find_rollout(root: &Path, thread_id: &str) -> Option<PathBuf> {
    let mut days: Vec<PathBuf> = child_dirs(root)
        .iter()
        .flat_map(|y| child_dirs(y))
        .flat_map(|m| child_dirs(&m))
        .collect();
    days.sort();
    let suffix = format!("-{thread_id}.jsonl");
    days.iter().rev().take(3).find_map(|day| {
        std::fs::read_dir(day)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(&suffix))
            })
    })
}

static REFUSED_LOG: Once = Once::new();

/// Read, audit and delete one turn's session log. `Ok` only when no tool ran.
pub(super) fn verify_and_discard(thread_id: Option<&str>) -> Result<()> {
    let unaudited = |why: String| {
        EngineError::Other(format!(
            "{WHO}: {why} — without the session log nothing proves the answer came from reasoning \
             rather than a tool, so it is not scored"
        ))
    };
    let thread = thread_id.ok_or_else(|| unaudited("the turn reported no thread id".into()))?;
    let root = sessions_root()
        .ok_or_else(|| unaudited("neither CODEX_HOME nor a home directory is set".into()))?;
    let path = find_rollout(&root, thread).ok_or_else(|| {
        unaudited(format!(
            "no session log for thread {thread} under '{}'",
            root.display()
        ))
    })?;
    let log = std::fs::read_to_string(&path)
        .map_err(|e| unaudited(format!("reading '{}': {e}", path.display())))?;
    let _ = std::fs::remove_file(&path);
    let audit = audit(&log);
    if !audit.executed.is_empty() {
        return Err(EngineError::Other(format!(
            "{WHO}: the model ran a tool during this call ({}) — its answer came from the tool, not \
             from reasoning, and is not scored. Every tool the adapter knows is disabled, so Codex \
             offered one it does not: add its feature to codex::posture::DISABLED_FEATURES",
            audit.executed.join(", ")
        )));
    }
    if !audit.refused.is_empty() {
        REFUSED_LOG.call_once(|| {
            eprintln!(
                "[engine] codex: a model attempted a disabled tool ({}); Codex refused it and the \
                 answer came from reasoning. Such a call carries one failed tool round in its tokens \
                 and latency",
                audit.refused.join(", ")
            )
        });
    }
    Ok(())
}

/// Delete a turn's session log without auditing it — for a turn that failed before it answered.
pub(super) fn discard(thread_id: Option<&str>) {
    if let (Some(thread), Some(root)) = (thread_id, sessions_root()) {
        if let Some(path) = find_rollout(&root, thread) {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real lines from a gpt-6-astra session with the code host disabled (codex-cli 0.154.0): the model
    /// reached for JavaScript and Codex refused it.
    const REFUSED_EXEC: &str = r#"{"type":"response_item","payload":{"type":"custom_tool_call","status":"completed","call_id":"call_xeoJVuPk8gyc6YXPW60MOieJ","name":"exec","input":"let sum=0;for(let n=10000;n<=10500;n++){}"}}
{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_xeoJVuPk8gyc6YXPW60MOieJ","output":"code-mode host is disabled"}}"#;

    /// A real line from a gpt-5.5 session: a hosted web search that completed.
    const WEB_SEARCH: &str = r#"{"type":"response_item","payload":{"type":"web_search_call","status":"completed","action":{"type":"search","query":"prime numbers between 10000 and 10500"}}}"#;

    /// Real lines from the session that started all this: the tool ran and returned the answer.
    const EXEC_RAN: &str = r#"{"type":"response_item","payload":{"type":"custom_tool_call","status":"completed","call_id":"call_1","name":"exec","input":"text(s)"}}
{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_1","output":[{"type":"input_text","text":"Script completed\nWall time 0.0 seconds\nOutput:\n"},{"type":"input_text","text":"563689"}]}}"#;

    #[test]
    fn a_refused_tool_is_not_a_tool_that_ran() {
        let a = audit(REFUSED_EXEC);
        assert_eq!(a.refused, vec!["exec"]);
        assert!(a.executed.is_empty());
    }

    /// **The case that started this.** The tool ran and its output is the answer.
    #[test]
    fn a_tool_that_returned_output_ran() {
        assert_eq!(audit(EXEC_RAN).executed, vec!["exec"]);
    }

    /// A hosted search has no local output to inspect; it ran unless it says it failed.
    #[test]
    fn a_hosted_web_search_ran() {
        assert_eq!(audit(WEB_SEARCH).executed, vec!["web_search_call"]);
        let failed = WEB_SEARCH.replace("\"completed\"", "\"failed\"");
        assert_eq!(audit(&failed).refused, vec!["web_search_call"]);
    }

    /// Fail closed: a call with no output line is not assumed refused.
    #[test]
    fn a_local_call_with_no_output_counts_as_ran() {
        let only_call = REFUSED_EXEC.lines().next().unwrap();
        assert_eq!(audit(only_call).executed, vec!["exec"]);
    }

    #[test]
    fn a_clean_turn_and_other_line_types_audit_as_nothing() {
        let clean = r#"{"type":"session_meta","payload":{"id":"x"}}
{"type":"response_item","payload":{"type":"message","role":"assistant"}}
not json"#;
        assert_eq!(audit(clean), ToolAudit::default());
    }

    #[test]
    fn the_rollout_is_found_in_the_newest_day_directories() {
        let root = std::env::temp_dir().join(format!("lt-codex-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (day, thread) in [("2026/09/13", "aaa"), ("2026/09/14", "bbb")] {
            let dir = root.join(day);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("rollout-2026-T10-{thread}.jsonl")), "").unwrap();
        }
        assert!(find_rollout(&root, "bbb")
            .unwrap()
            .ends_with("rollout-2026-T10-bbb.jsonl"));
        assert!(
            find_rollout(&root, "aaa").is_some(),
            "a call across midnight"
        );
        assert!(find_rollout(&root, "zzz").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An unauditable turn is an error, never a scored answer.
    #[test]
    fn a_turn_with_no_thread_id_is_refused() {
        let err = verify_and_discard(None).unwrap_err();
        assert!(err.to_string().contains("not scored"), "{err}");
    }
}
