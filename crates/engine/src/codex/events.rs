//! Reading `codex exec --json`: one JSON object per line.
//!
//! **These shapes were captured from real runs** (`codex-cli` 0.139.0 and 0.154.0, 2026-09-14), not
//! written from documentation. That distinction is the reason this comment exists: the only other
//! Codex reader in the wider codebase (athena-portable's `CodexDialect`) was built against a fixture
//! its own header calls synthetic, and never ran against the binary.
//!
//! A successful turn is four lines:
//! `thread.started` → `turn.started` → `item.completed` (`item.type == "agent_message"`, the answer)
//! → `turn.completed` (the usage). A failed one replaces the last two with `error` and `turn.failed`,
//! whose `message` is itself a JSON document when the failure came from the API.

use serde_json::Value;

/// Token accounting from `turn.completed.usage`. Every field is optional because the shape has
/// already drifted between releases (0.139 has no `cache_write_input_tokens`; 0.154 does).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Usage {
    /// Includes the cached share, as the OpenAI usage convention does.
    pub(crate) input: Option<u64>,
    pub(crate) cached_input: Option<u64>,
    /// Includes the reasoning share: a 24-token turn reported 15 of them as reasoning.
    pub(crate) output: Option<u64>,
    pub(crate) reasoning: Option<u64>,
}

/// What one `codex exec` turn produced.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Turn {
    /// From `thread.started`: the id that names the turn's session log, which is the only record of
    /// the tools it called.
    pub(crate) thread_id: Option<String>,
    /// The **last** `agent_message` of the turn. A tool-less turn emits one; if a release ever emits
    /// a preamble before the answer, the answer is the one that closed the turn.
    pub(crate) text: Option<String>,
    pub(crate) usage: Option<Usage>,
    /// The first failure the turn reported, unwrapped from the API's JSON envelope where it has one.
    pub(crate) error: Option<String>,
    /// The HTTP status inside that envelope, when it carried one — what separates a retryable rate
    /// limit from a request the model refused.
    pub(crate) status: Option<u16>,
}

/// Read a whole `--json` stdout. Lines that are not JSON, and event types this build does not use,
/// are skipped: Codex adds event kinds between releases, and a reader that failed on the first
/// unknown one would turn every upgrade into an outage.
pub(crate) fn read(stdout: &str) -> Turn {
    let mut turn = Turn::default();
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("thread.started") => {
                turn.thread_id = v
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }
            Some("item.completed") => {
                let item = &v["item"];
                if item["type"] == "agent_message" {
                    if let Some(t) = item["text"].as_str() {
                        turn.text = Some(t.to_string());
                    }
                }
            }
            Some("turn.completed") => turn.usage = Some(usage_of(&v["usage"])),
            Some("error") => {
                // `Reconnecting... 2/5 (stream disconnected before completion ...)` is Codex narrating
                // its own retry, not a failure: the turn usually goes on to complete. Treating it as
                // fatal errored 3 of 40 calls in the first tool-free pilot.
                let narration = v
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|m| m.starts_with("Reconnecting"));
                if !narration {
                    record_error(&mut turn, v.get("message"));
                }
            }
            Some("turn.failed") => record_error(&mut turn, v.pointer("/error/message")),
            _ => {}
        }
    }
    turn
}

fn usage_of(u: &Value) -> Usage {
    let n = |k: &str| u.get(k).and_then(Value::as_u64);
    Usage {
        input: n("input_tokens"),
        cached_input: n("cached_input_tokens"),
        output: n("output_tokens"),
        reasoning: n("reasoning_output_tokens"),
    }
}

/// Keep the first failure. `error` and `turn.failed` repeat the same message, so the second is noise.
fn record_error(turn: &mut Turn, message: Option<&Value>) {
    if turn.error.is_some() {
        return;
    }
    let raw = message
        .and_then(Value::as_str)
        .unwrap_or("codex reported an error with no message");
    match serde_json::from_str::<Value>(raw) {
        Ok(inner) if inner.is_object() => {
            turn.status = inner
                .get("status")
                .and_then(Value::as_u64)
                .and_then(|s| u16::try_from(s).ok());
            turn.error = Some(
                inner
                    .pointer("/error/message")
                    .or_else(|| inner.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or(raw)
                    .to_string(),
            );
        }
        _ => turn.error = Some(raw.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real successful turn, byte for byte, from `codex-cli` 0.154.0 on gpt-5.5 at `low`.
    const OK_0154: &str = r#"{"type":"thread.started","thread_id":"01a0a0b1-ad62-7cc0-8884-2911b047756c"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"0.05"}}
{"type":"turn.completed","usage":{"input_tokens":13327,"cached_input_tokens":5504,"cache_write_input_tokens":0,"output_tokens":24,"reasoning_output_tokens":15}}"#;

    /// A real failed turn from `codex-cli` 0.139.0: the API refused the model, and the message is a
    /// JSON document inside a JSON string.
    const FAILED_0139: &str = r#"{"type":"thread.started","thread_id":"01a0a0b3-0f8e-7020-8cce-30c8cbc127a8"}
{"type":"turn.started"}
{"type":"error","message":"{\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'gpt-5.6-luna' model requires a newer version of Codex. Please upgrade to the latest app or CLI and try again.\"}}"}
{"type":"turn.failed","error":{"message":"{\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'gpt-5.6-luna' model requires a newer version of Codex. Please upgrade to the latest app or CLI and try again.\"}}"}}"#;

    #[test]
    fn a_real_turn_yields_the_answer_and_the_reasoning_split() {
        let t = read(OK_0154);
        assert_eq!(
            t.thread_id.as_deref(),
            Some("01a0a0b1-ad62-7cc0-8884-2911b047756c"),
            "the audit finds the session log by this id"
        );
        assert_eq!(t.text.as_deref(), Some("0.05"));
        assert_eq!(
            t.usage,
            Some(Usage {
                input: Some(13327),
                cached_input: Some(5504),
                output: Some(24),
                reasoning: Some(15),
            })
        );
        assert!(t.error.is_none());
    }

    /// The API's own message is unwrapped from its envelope, with the status that classifies it —
    /// an operator should read "requires a newer version of Codex", not a wall of escaped JSON.
    #[test]
    fn a_real_failure_is_unwrapped_and_carries_its_status() {
        let t = read(FAILED_0139);
        assert_eq!(t.status, Some(400));
        assert_eq!(
            t.error.as_deref(),
            Some(
                "The 'gpt-5.6-luna' model requires a newer version of Codex. Please upgrade to the \
                 latest app or CLI and try again."
            )
        );
        assert!(t.text.is_none() && t.usage.is_none());
    }

    /// The 0.139 usage block has no `cache_write_input_tokens`; an absent field is unknown, and its
    /// absence must not cost the fields that are there.
    #[test]
    fn an_older_usage_shape_still_reads() {
        let t = read(
            r#"{"type":"turn.completed","usage":{"input_tokens":18902,"cached_input_tokens":4480,"output_tokens":51,"reasoning_output_tokens":40}}"#,
        );
        let u = t.usage.unwrap();
        assert_eq!((u.output, u.reasoning), (Some(51), Some(40)));
    }

    #[test]
    fn the_last_agent_message_is_the_answer_and_noise_is_skipped() {
        let t = read(
            "not json at all\n\
             {\"type\":\"some.future.event\",\"x\":1}\n\
             {\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"thinking out loud\"}}\n\
             {\"type\":\"item.completed\",\"item\":{\"type\":\"reasoning\",\"text\":\"hidden\"}}\n\
             {\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"42\"}}\n",
        );
        assert_eq!(t.text.as_deref(), Some("42"));
    }

    /// **Found by the first tool-free pilot.** Codex reports its own reconnect attempts as `error`
    /// events and then completes the turn; the message is the real one it printed.
    #[test]
    fn a_reconnect_notice_is_narration_not_a_failure() {
        let t = read(
            r#"{"type":"thread.started","thread_id":"t1"}
{"type":"turn.started"}
{"type":"error","message":"Reconnecting... 2/5 (stream disconnected before completion: WebSocket protocol error: Connection reset without closing handshake)"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"8976"}}
{"type":"turn.completed","usage":{"input_tokens":7000,"output_tokens":900,"reasoning_output_tokens":880}}"#,
        );
        assert!(t.error.is_none(), "{:?}", t.error);
        assert_eq!(t.text.as_deref(), Some("8976"));
        assert!(t.usage.is_some());
    }

    #[test]
    fn a_plain_text_error_is_kept_verbatim() {
        let t = read(r#"{"type":"error","message":"You've hit your usage limit."}"#);
        assert_eq!(t.error.as_deref(), Some("You've hit your usage limit."));
        assert_eq!(t.status, None);
        assert_eq!(read("").error, None);
    }
}
