//! The argv for one isolated, **tool-less** `codex exec` turn — and why every flag is there.
//!
//! Tool-less is the part that was learned the hard way. `--sandbox read-only` stops a model writing
//! to disk; it does not stop it *computing*. On 2026-09-14 gpt-6-astra answered "sum every prime
//! between 10000 and 10500" with 0 reasoning tokens by calling a JavaScript `exec` tool, and
//! gpt-5.5 answered it with a hosted web search for the prime list. Neither call showed a single tool
//! event on the `--json` stream. A benchmark of reasoning effort that lets the model run code measures
//! whether it thought to run code — so every tool feature Codex exposes is switched off here, and
//! [`super::audit`] proves after the fact that none ran.

use std::path::Path;

use lighttrack_core::Effort;

use crate::{EngineError, Result};

/// Longest system prompt passed through argv. It travels as a `-c developer_instructions=` value,
/// and Windows caps a whole command line near 32k characters; a longer prompt is refused by name
/// here rather than truncated by the OS into a different prompt — and the generation path folds
/// it into the user turn *before* reaching this check (see `fold_oversized_system`), so the
/// refusal is the guard for a caller that builds argv directly, not the outcome an app sees.
pub(super) const MAX_INSTRUCTIONS_CHARS: usize = 16_000;

/// Every Codex feature (0.154.0, `codex features list`) that hands the model a tool. Disabling a
/// feature that a release has removed is harmless; *missing* one a release adds is not, which is
/// what the post-call audit exists to catch — its error names this list.
pub(super) const DISABLED_FEATURES: &[&str] = &[
    // Local execution: the JavaScript `exec` host, the shell, and the unified exec runner.
    "code_mode_host",
    "shell_tool",
    "unified_exec",
    "unified_exec_tty",
    // Everything else that acts or reaches outside the conversation.
    "view_image",
    "multi_agent",
    "apps",
    "browser_use",
    "browser_use_external",
    "in_app_browser",
    "computer_use",
    "image_generation",
    "plugins",
    "remote_plugin",
    "skill_search",
    "skill_mcp_dependency_install",
    "tool_suggest",
    "sleep_tool",
    "goals",
    "hooks",
    "workspace_dependencies",
];

/// Web search is a **hosted** tool — it runs on OpenAI's side, so no local feature flag reaches it.
/// It is switched off in configuration, under both spellings Codex has used; each alone produced
/// zero tool calls in a live probe, and the audit covers a release that renames it again.
const WEB_SEARCH_OFF: [&str; 2] = ["web_search=\"disabled\"", "tools.web_search=false"];

/// The argv for one turn. The prompt is not here: it travels over stdin (`-`).
///
/// No `--ephemeral`: the session log is the only record of which tools a turn called, so it must be
/// written. [`super::audit`] reads it and deletes it, which restores the property `--ephemeral` gave.
pub(super) fn argv(
    model: &str,
    system_prompt: Option<&str>,
    effort: Option<Effort>,
    schema_file: Option<&Path>,
) -> Result<Vec<String>> {
    let mut a: Vec<String> = [
        "exec",
        "--json",
        "--skip-git-repo-check",
        "--ignore-user-config",
        "--ignore-rules",
        "--sandbox",
        "read-only",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for feature in DISABLED_FEATURES {
        a.push("--disable".into());
        a.push((*feature).to_string());
    }
    for setting in WEB_SEARCH_OFF {
        a.push("-c".into());
        a.push(setting.to_string());
    }
    a.push("--model".into());
    a.push(model.to_string());
    if let Some(level) = effort {
        // Quoted, so it is parsed as a TOML string rather than falling back to a bare literal.
        a.push("-c".into());
        a.push(format!("model_reasoning_effort=\"{}\"", level.as_str()));
    }
    if let Some(sys) = system_prompt {
        if sys.chars().count() > MAX_INSTRUCTIONS_CHARS {
            return Err(EngineError::Other(format!(
                "the codex adapter passes a system prompt on the command line and caps it at \
                 {MAX_INSTRUCTIONS_CHARS} characters; this one is {} — a longer one would be \
                 truncated by the OS into a different prompt",
                sys.chars().count()
            )));
        }
        // A JSON string literal is a valid TOML basic string, so quotes and newlines survive intact.
        a.push("-c".into());
        a.push(format!(
            "developer_instructions={}",
            serde_json::to_string(sys)?
        ));
    }
    if let Some(path) = schema_file {
        a.push("--output-schema".into());
        a.push(path.display().to_string());
    }
    a.push("-".into());
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_pair(a: &[String], flag: &str, value: &str) -> bool {
        a.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    /// **The isolation posture, asserted where it is written.** Every one of these keeps something
    /// of the operator's out of the measurement: their config (default model and effort), their
    /// rules, write access to a disk.
    #[test]
    fn every_call_is_isolated_read_only_and_reads_its_prompt_from_stdin() {
        let a = argv("gpt-5.5", None, None, None).unwrap();
        assert_eq!(a[0], "exec");
        for flag in [
            "--json",
            "--skip-git-repo-check",
            "--ignore-user-config",
            "--ignore-rules",
        ] {
            assert!(a.iter().any(|x| x == flag), "missing {flag}: {a:?}");
        }
        assert!(has_pair(&a, "--sandbox", "read-only"));
        assert!(has_pair(&a, "--model", "gpt-5.5"));
        assert_eq!(a.last().map(String::as_str), Some("-"), "prompt over stdin");
        assert!(!a.iter().any(|x| x.starts_with("model_reasoning_effort")));
    }

    /// **The regression this module exists for.** Code execution, the shell and web search were all
    /// live under `--sandbox read-only`, and two models used them to answer arithmetic without
    /// reasoning. Each is switched off by the flag that reaches it.
    #[test]
    fn every_tool_the_model_used_is_switched_off() {
        let a = argv("gpt-6-astra", None, Some(Effort::Low), None).unwrap();
        for feature in ["code_mode_host", "shell_tool", "unified_exec"] {
            assert!(
                has_pair(&a, "--disable", feature),
                "{feature} left on: {a:?}"
            );
        }
        for feature in DISABLED_FEATURES {
            assert!(has_pair(&a, "--disable", feature), "{feature} left on");
        }
        assert!(has_pair(&a, "-c", "web_search=\"disabled\""), "{a:?}");
        assert!(has_pair(&a, "-c", "tools.web_search=false"), "{a:?}");
    }

    /// The session log is the audit's only evidence, so the turn must not be ephemeral.
    #[test]
    fn the_session_log_is_kept_for_the_audit() {
        let a = argv("gpt-5.5", None, None, None).unwrap();
        assert!(!a.iter().any(|x| x == "--ephemeral"), "{a:?}");
    }

    #[test]
    fn every_level_travels_as_a_quoted_toml_string() {
        for level in Effort::ALL {
            let a = argv("gpt-5.5", None, Some(level), None).unwrap();
            assert!(
                has_pair(
                    &a,
                    "-c",
                    &format!("model_reasoning_effort=\"{}\"", level.as_str())
                ),
                "{a:?}"
            );
        }
    }

    /// A system prompt with quotes and newlines arrives intact: the value is a JSON string literal,
    /// which TOML reads as the same string.
    #[test]
    fn a_system_prompt_survives_quotes_and_newlines() {
        let sys = "Answer \"tersely\".\nNo preamble.\tEver.";
        let a = argv("gpt-5.5", Some(sys), None, None).unwrap();
        let v = a
            .iter()
            .find_map(|x| x.strip_prefix("developer_instructions="))
            .expect("instructions passed");
        assert_eq!(serde_json::from_str::<String>(v).unwrap(), sys);
    }

    #[test]
    fn an_oversized_system_prompt_is_refused_by_name() {
        let huge = "x".repeat(MAX_INSTRUCTIONS_CHARS + 1);
        let err = argv("gpt-5.5", Some(&huge), None, None).unwrap_err();
        assert!(err.to_string().contains("caps it at"), "{err}");
        assert!(argv(
            "gpt-5.5",
            Some(&"x".repeat(MAX_INSTRUCTIONS_CHARS)),
            None,
            None
        )
        .is_ok());
    }

    #[test]
    fn a_schema_is_passed_by_file_before_the_stdin_marker() {
        let a = argv("gpt-5.5", None, None, Some(Path::new("s.json"))).unwrap();
        assert!(has_pair(&a, "--output-schema", "s.json"));
        assert_eq!(a.last().map(String::as_str), Some("-"));
    }
}
