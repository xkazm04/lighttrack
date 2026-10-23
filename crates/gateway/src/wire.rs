//! The OpenAI chat-completions wire shape, folded onto the engine's [`ChatRequest`].
//!
//! Text turns, tool definitions, assistant `tool_calls` and `tool` results all travel; the engine
//! decides per provider what it can honour (the OpenAI-shaped providers take everything natively,
//! Gemini and the Anthropic API take plain turns, the CLIs take one rendered prompt and refuse
//! tools). The response says when a conversation was rendered (`lighttrack.transcript`), so an
//! app that sent ten turns to a CLI knows they went as text.

use serde::Deserialize;
use serde_json::Value;

use lighttrack_engine::{ChatRequest, ChatRole, ChatTurn};

#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    /// A route name from `gateway.toml`, or a literal `provider/model[@effort]`.
    pub model: String,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub response_format: Option<ResponseFormat>,
    /// Anything else the SDK sends (`temperature`, `max_tokens`, …) is accepted and ignored: the
    /// CLIs expose no such knobs, and refusing them would break every default client.
    #[serde(flatten)]
    pub _rest: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub role: String,
    /// A string, an array of `{type: "text", text}` parts, or null beside `tool_calls`.
    #[serde(default)]
    pub content: Value,
    #[serde(default)]
    pub tool_calls: Option<Value>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResponseFormat {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub json_schema: Option<JsonSchemaFormat>,
}

#[derive(Debug, Deserialize)]
pub struct JsonSchemaFormat {
    pub schema: Value,
}

/// The engine's view of a request, plus what the response needs to say about the folding.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub req: ChatRequest,
    /// True when more than one non-system turn was sent — a CLI target will see them rendered.
    pub transcript: bool,
}

/// Text of a message's content; refuses image/audio parts, which no provider path here accepts.
fn text_of(content: &Value) -> Result<String, String> {
    match content {
        Value::Null => Ok(String::new()),
        Value::String(s) => Ok(s.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            if !out.is_empty() {
                                out.push('\n');
                            }
                            out.push_str(t);
                        }
                    }
                    other => {
                        return Err(format!(
                            "unsupported content part type {:?}: the gateway forwards text only",
                            other.unwrap_or("?")
                        ))
                    }
                }
            }
            Ok(out)
        }
        other => Err(format!(
            "message content must be a string or parts array, got {other}"
        )),
    }
}

impl ChatCompletionRequest {
    pub fn prepare(&self) -> Result<Prepared, String> {
        if self.messages.is_empty() {
            return Err("messages must not be empty".into());
        }
        let mut system_parts = Vec::new();
        let mut turns: Vec<ChatTurn> = Vec::new();
        for m in &self.messages {
            let content = text_of(&m.content)?;
            let role = match m.role.as_str() {
                "system" | "developer" => {
                    system_parts.push(content);
                    continue;
                }
                "user" => ChatRole::User,
                "assistant" => ChatRole::Assistant,
                "tool" => ChatRole::Tool,
                other => return Err(format!("unsupported message role '{other}'")),
            };
            if role == ChatRole::Tool && m.tool_call_id.is_none() {
                return Err("a tool message needs a tool_call_id".into());
            }
            turns.push(ChatTurn {
                role,
                content,
                tool_calls: m.tool_calls.clone().filter(|v| !v.is_null()),
                tool_call_id: m.tool_call_id.clone(),
            });
        }
        if turns.is_empty() {
            return Err("at least one user message is required".into());
        }
        let transcript = turns.len() > 1;
        let mut system = if system_parts.is_empty() {
            None
        } else {
            Some(system_parts.join("\n\n"))
        };
        let schema = match &self.response_format {
            Some(f) if f.kind == "json_schema" => Some(
                f.json_schema
                    .as_ref()
                    .map(|j| j.schema.clone())
                    .ok_or("response_format.json_schema.schema is required")?,
            ),
            // `json_object` asks for syntax without a shape; the CLIs have no such knob, so the
            // system prompt carries the request and the caller validates as it would anyway.
            Some(f) if f.kind == "json_object" => {
                let line = "Respond with a single JSON object and nothing else.";
                system = Some(match system {
                    Some(s) => format!("{s}\n\n{line}"),
                    None => line.to_string(),
                });
                None
            }
            Some(f) if f.kind == "text" => None,
            Some(f) => return Err(format!("unsupported response_format type '{}'", f.kind)),
            None => None,
        };
        let tools = if self.tools.is_empty() {
            None
        } else {
            Some(Value::Array(self.tools.clone()))
        };
        Ok(Prepared {
            req: ChatRequest {
                system,
                turns,
                schema,
                tools,
                tool_choice: self.tool_choice.clone(),
            },
            transcript,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(v: Value) -> ChatCompletionRequest {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_single_user_turn_and_a_system_prompt_map_one_to_one() {
        let p = req(json!({"model": "x", "messages": [
            {"role": "system", "content": "be terse"},
            {"role": "user", "content": "hi"}
        ]}))
        .prepare()
        .unwrap();
        assert_eq!(p.req.system.as_deref(), Some("be terse"));
        assert_eq!(p.req.turns, vec![ChatTurn::user("hi")]);
        assert!(!p.transcript);
        assert_eq!(p.req.render().unwrap(), "hi");
    }

    #[test]
    fn a_conversation_keeps_its_turns_and_is_flagged() {
        let p = req(json!({"model": "x", "messages": [
            {"role": "user", "content": "a"},
            {"role": "assistant", "content": "b"},
            {"role": "user", "content": [{"type": "text", "text": "c"}]}
        ]}))
        .prepare()
        .unwrap();
        assert!(p.transcript);
        assert_eq!(p.req.turns.len(), 3);
        assert_eq!(p.req.turns[1].role, ChatRole::Assistant);
        assert!(p.req.render().unwrap().ends_with("User: c\n\nAssistant:"));
    }

    #[test]
    fn tool_definitions_calls_and_results_travel() {
        let p = req(json!({"model": "x", "messages": [
            {"role": "user", "content": "time?"},
            {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function",
                "function": {"name": "now", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "12:00"}
        ], "tools": [{"type": "function", "function": {"name": "now"}}], "tool_choice": "auto"}))
        .prepare()
        .unwrap();
        assert!(p.req.has_tools());
        assert_eq!(p.req.tool_choice, Some(json!("auto")));
        assert_eq!(p.req.turns[2].role, ChatRole::Tool);
        assert_eq!(p.req.turns[2].tool_call_id.as_deref(), Some("c1"));
        assert!(p.req.turns[1].tool_calls.is_some());
    }

    #[test]
    fn json_schema_travels_and_json_object_becomes_an_instruction() {
        let p = req(json!({"model": "x", "messages": [{"role": "user", "content": "hi"}],
            "response_format": {"type": "json_schema", "json_schema": {"name": "n", "schema": {"type": "object"}}}}))
        .prepare()
        .unwrap();
        assert_eq!(p.req.schema, Some(json!({"type": "object"})));
        let p = req(
            json!({"model": "x", "messages": [{"role": "user", "content": "hi"}],
            "response_format": {"type": "json_object"}}),
        )
        .prepare()
        .unwrap();
        assert!(p.req.schema.is_none());
        assert!(p.req.system.unwrap().contains("single JSON object"));
    }

    #[test]
    fn image_parts_unknown_roles_and_orphan_tool_results_are_refused() {
        assert!(req(json!({"model": "x", "messages": [
            {"role": "user", "content": [{"type": "image_url", "image_url": {"url": "x"}}]}]}))
        .prepare()
        .is_err());
        assert!(
            req(json!({"model": "x", "messages": [{"role": "function", "content": "x"}]}))
                .prepare()
                .is_err()
        );
        assert!(
            req(json!({"model": "x", "messages": [{"role": "tool", "content": "x"}]}))
                .prepare()
                .is_err()
        );
        assert!(req(json!({"model": "x", "messages": []}))
            .prepare()
            .is_err());
    }
}
