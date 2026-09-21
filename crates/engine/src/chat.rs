//! A conversation as a generation input: system + turns (+ tools), and how each provider shape
//! takes it.
//!
//! The judge and the benchmark only ever needed `(system, input, schema)`, and every adapter was
//! built for that. The gateway fronts apps whose SDKs send a message array — and, on the
//! OpenAI-shaped providers, tool definitions and tool results. This is the one place that vocabulary
//! is defined; each adapter asks for its own wire view (`openai_messages`, `gemini_contents`,
//! `anthropic_messages`), and a CLI that takes a single prompt asks for [`ChatRequest::render`].
//!
//! What a provider cannot honour is an **error before the request**, never a silent drop: a tool
//! list sent to a path that ignores tools would come back as prose the caller believes was a
//! tool-aware answer.

use serde_json::{json, Value};

use crate::{EngineError, GenOutcome, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    /// A tool result, answering an assistant turn's `tool_calls` (OpenAI shape).
    Tool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatTurn {
    pub role: ChatRole,
    pub content: String,
    /// On an assistant turn: the `tool_calls` array the model produced earlier (OpenAI shape).
    pub tool_calls: Option<Value>,
    /// On a tool turn: which call this result answers.
    pub tool_call_id: Option<String>,
}

impl ChatTurn {
    pub fn user(content: impl Into<String>) -> ChatTurn {
        ChatTurn {
            role: ChatRole::User,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> ChatTurn {
        ChatTurn {
            role: ChatRole::Assistant,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatRequest {
    pub system: Option<String>,
    pub turns: Vec<ChatTurn>,
    /// A JSON schema the answer must satisfy (the structured-output path).
    pub schema: Option<Value>,
    /// OpenAI-shaped `tools` array, when the caller wants the model to be able to call some.
    pub tools: Option<Value>,
    pub tool_choice: Option<Value>,
}

impl ChatRequest {
    /// The judge/benchmark shape: one user turn.
    pub fn single(system: Option<&str>, input: &str, schema: Option<&Value>) -> ChatRequest {
        ChatRequest {
            system: system.map(str::to_string),
            turns: vec![ChatTurn::user(input)],
            schema: schema.cloned(),
            tools: None,
            tool_choice: None,
        }
    }

    pub fn has_tools(&self) -> bool {
        self.tools
            .as_ref()
            .and_then(Value::as_array)
            .is_some_and(|t| !t.is_empty())
    }

    fn has_tool_turns(&self) -> bool {
        self.turns
            .iter()
            .any(|t| t.role == ChatRole::Tool || t.tool_calls.is_some())
    }

    /// The whole conversation as one prompt, for a CLI that takes exactly one. A single user turn
    /// is returned verbatim, so the judge path is byte-identical to what it always sent.
    pub fn render(&self) -> Result<String> {
        if self.has_tools() || self.has_tool_turns() {
            return Err(EngineError::Other(
                "this provider takes a single prompt and cannot run tools; send tool-using \
                 conversations to an OpenAI-shaped provider"
                    .into(),
            ));
        }
        if self.turns.len() == 1 && self.turns[0].role == ChatRole::User {
            return Ok(self.turns[0].content.clone());
        }
        let mut s = String::from(
            "The following is a conversation so far. Reply as the assistant to the last user \
             turn.\n\n",
        );
        for t in &self.turns {
            let label = match t.role {
                ChatRole::User => "User",
                ChatRole::Assistant => "Assistant",
                ChatRole::Tool => unreachable!("refused above"),
            };
            s.push_str(&format!("{label}: {}\n\n", t.content));
        }
        s.push_str("Assistant:");
        Ok(s)
    }

    /// The OpenAI `messages` array: system first, then every turn including tool traffic.
    pub fn openai_messages(&self) -> Vec<Value> {
        let mut out = Vec::with_capacity(self.turns.len() + 1);
        if let Some(sys) = &self.system {
            out.push(json!({ "role": "system", "content": sys }));
        }
        for t in &self.turns {
            let mut m = match t.role {
                ChatRole::User => json!({ "role": "user", "content": t.content }),
                ChatRole::Assistant => json!({ "role": "assistant", "content": t.content }),
                ChatRole::Tool => json!({
                    "role": "tool",
                    "content": t.content,
                    "tool_call_id": t.tool_call_id,
                }),
            };
            if let Some(tc) = &t.tool_calls {
                m["tool_calls"] = tc.clone();
                // OpenAI wants `content: null` (not "") beside tool_calls on an assistant turn.
                if t.content.is_empty() {
                    m["content"] = Value::Null;
                }
            }
            out.push(m);
        }
        out
    }

    /// Gemini `contents`: `user` / `model` parts. No tool vocabulary here (yet) — refused.
    pub fn gemini_contents(&self) -> Result<Vec<Value>> {
        self.refuse_tools("gemini")?;
        Ok(self
            .turns
            .iter()
            .map(|t| {
                let role = if t.role == ChatRole::User {
                    "user"
                } else {
                    "model"
                };
                json!({ "role": role, "parts": [{ "text": t.content }] })
            })
            .collect())
    }

    /// Anthropic Messages `messages`: `user` / `assistant`. Caller tools are refused — the schema
    /// path already uses forced tool use, and the two cannot share one request honestly.
    pub fn anthropic_messages(&self) -> Result<Vec<Value>> {
        self.refuse_tools("anthropic")?;
        Ok(self
            .turns
            .iter()
            .map(|t| {
                let role = if t.role == ChatRole::User {
                    "user"
                } else {
                    "assistant"
                };
                json!({ "role": role, "content": t.content })
            })
            .collect())
    }

    fn refuse_tools(&self, who: &str) -> Result<()> {
        if self.has_tools() || self.has_tool_turns() {
            return Err(EngineError::Other(format!(
                "the {who} adapter does not pass tools through; send tool-using conversations to \
                 an OpenAI-shaped provider (openai, openrouter)"
            )));
        }
        Ok(())
    }
}

/// Whether a provider id can take caller-defined tools. Only the OpenAI wire shape is passed
/// through as-is; everything else refuses before the request.
pub fn supports_tools(provider: &str) -> bool {
    provider == "openrouter"
        || lighttrack_core::family_of(provider) == lighttrack_core::ProviderFamily::OpenAi
            && provider != "codex"
}

/// One chat answer: the text outcome plus what only a chat surface carries.
#[derive(Debug, Clone)]
pub struct ChatOutcome {
    pub gen: GenOutcome,
    /// The model asked for tools to be run (OpenAI shape). `gen.output` may be empty then.
    pub tool_calls: Option<Value>,
    /// The provider's own stop reason, when it states one (`stop`, `tool_calls`, …).
    pub finish_reason: Option<String>,
}

impl ChatOutcome {
    pub fn text(gen: GenOutcome) -> ChatOutcome {
        ChatOutcome {
            gen,
            tool_calls: None,
            finish_reason: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_user_turn_renders_verbatim_and_a_conversation_is_labelled() {
        let r = ChatRequest::single(Some("s"), "hi", None);
        assert_eq!(r.render().unwrap(), "hi");
        let r = ChatRequest {
            turns: vec![
                ChatTurn::user("a"),
                ChatTurn::assistant("b"),
                ChatTurn::user("c"),
            ],
            ..Default::default()
        };
        let s = r.render().unwrap();
        assert!(s.contains("User: a\n\nAssistant: b\n\nUser: c\n\nAssistant:"));
    }

    #[test]
    fn openai_messages_carry_tool_traffic_and_the_other_shapes_refuse_it() {
        let r = ChatRequest {
            system: Some("s".into()),
            turns: vec![
                ChatTurn::user("what time"),
                ChatTurn {
                    role: ChatRole::Assistant,
                    content: String::new(),
                    tool_calls: Some(json!([{ "id": "c1", "type": "function",
                        "function": { "name": "now", "arguments": "{}" } }])),
                    tool_call_id: None,
                },
                ChatTurn {
                    role: ChatRole::Tool,
                    content: "12:00".into(),
                    tool_calls: None,
                    tool_call_id: Some("c1".into()),
                },
            ],
            tools: Some(json!([{ "type": "function", "function": { "name": "now" } }])),
            ..Default::default()
        };
        let m = r.openai_messages();
        assert_eq!(m.len(), 4);
        assert_eq!(m[2]["content"], Value::Null);
        assert_eq!(m[3]["tool_call_id"], "c1");
        assert!(r.render().is_err());
        assert!(r.gemini_contents().is_err());
        assert!(r.anthropic_messages().is_err());
    }

    #[test]
    fn gemini_and_anthropic_take_plain_turns_natively() {
        let r = ChatRequest {
            turns: vec![
                ChatTurn::user("a"),
                ChatTurn::assistant("b"),
                ChatTurn::user("c"),
            ],
            ..Default::default()
        };
        let g = r.gemini_contents().unwrap();
        assert_eq!(g[1]["role"], "model");
        let a = r.anthropic_messages().unwrap();
        assert_eq!(a[1]["role"], "assistant");
    }

    #[test]
    fn only_openai_shaped_providers_take_tools() {
        assert!(supports_tools("openai"));
        assert!(supports_tools("openrouter"));
        assert!(supports_tools("azure-openai"));
        assert!(!supports_tools("codex"));
        assert!(!supports_tools("anthropic"));
        assert!(!supports_tools("google"));
    }
}
