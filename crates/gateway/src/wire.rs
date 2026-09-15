//! The OpenAI chat-completions wire shape, and how it folds onto the engine's one-turn call.
//!
//! The engine generates from `(system_prompt, input, schema)` — the shape a benchmark case has —
//! because both CLIs it fronts take one prompt, not a message array. So a conversation is
//! rendered into a single input here, and the response says so (`lighttrack.transcript`): an app
//! that sends ten turns gets them all, as text, not silently truncated to the last one.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// A route name from `gateway.toml`, or a literal `provider/model[@effort]`.
    pub model: String,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub response_format: Option<ResponseFormat>,
    /// Anything else the SDK sends (`temperature`, `max_tokens`, …) is accepted and ignored: the
    /// CLIs expose no such knobs, and refusing them would break every default client.
    #[serde(flatten)]
    pub _rest: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    pub role: String,
    /// A string, or an array of `{type: "text", text}` parts.
    #[serde(default)]
    pub content: Value,
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

/// The engine's view of a request.
#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    pub system: Option<String>,
    pub input: String,
    pub schema: Option<Value>,
    /// True when more than one non-system turn was rendered into `input`.
    pub transcript: bool,
}

/// Text of a message's content; refuses image/audio parts, which no CLI path accepts.
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

impl ChatRequest {
    pub fn prompt(&self) -> Result<Prompt, String> {
        if self.messages.is_empty() {
            return Err("messages must not be empty".into());
        }
        let mut system_parts = Vec::new();
        let mut turns: Vec<(String, String)> = Vec::new();
        for m in &self.messages {
            let text = text_of(&m.content)?;
            match m.role.as_str() {
                "system" | "developer" => system_parts.push(text),
                "user" | "assistant" => turns.push((m.role.clone(), text)),
                other => return Err(format!("unsupported message role '{other}'")),
            }
        }
        if turns.is_empty() {
            return Err("at least one user message is required".into());
        }
        let transcript = turns.len() > 1;
        let input = if transcript {
            let mut s = String::from(
                "The following is a conversation so far. Reply as the assistant to the last \
                 user turn.\n\n",
            );
            for (role, text) in &turns {
                let label = if role == "user" { "User" } else { "Assistant" };
                s.push_str(&format!("{label}: {text}\n\n"));
            }
            s.push_str("Assistant:");
            s
        } else {
            turns[0].1.clone()
        };
        let system = if system_parts.is_empty() {
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
            Some(f) if f.kind == "json_object" => None,
            Some(f) if f.kind == "text" => None,
            Some(f) => return Err(format!("unsupported response_format type '{}'", f.kind)),
            None => None,
        };
        let system = match (&self.response_format, system) {
            (Some(f), s) if f.kind == "json_object" => Some(match s {
                Some(s) => format!("{s}\n\nRespond with a single JSON object and nothing else."),
                None => "Respond with a single JSON object and nothing else.".to_string(),
            }),
            (_, s) => s,
        };
        Ok(Prompt {
            system,
            input,
            schema,
            transcript,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(v: Value) -> ChatRequest {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_single_user_turn_is_the_input_verbatim_and_system_is_the_system() {
        let p = req(json!({"model": "x", "messages": [
            {"role": "system", "content": "be terse"},
            {"role": "user", "content": "hi"}
        ]}))
        .prompt()
        .unwrap();
        assert_eq!(p.system.as_deref(), Some("be terse"));
        assert_eq!(p.input, "hi");
        assert!(!p.transcript);
    }

    #[test]
    fn a_conversation_is_rendered_and_flagged() {
        let p = req(json!({"model": "x", "messages": [
            {"role": "user", "content": "a"},
            {"role": "assistant", "content": "b"},
            {"role": "user", "content": [{"type": "text", "text": "c"}]}
        ]}))
        .prompt()
        .unwrap();
        assert!(p.transcript);
        assert!(p.input.contains("User: a"));
        assert!(p.input.contains("Assistant: b"));
        assert!(p.input.ends_with("User: c\n\nAssistant:"));
    }

    #[test]
    fn json_schema_travels_and_json_object_becomes_an_instruction() {
        let p = req(json!({"model": "x", "messages": [{"role": "user", "content": "hi"}],
            "response_format": {"type": "json_schema", "json_schema": {"name": "n", "schema": {"type": "object"}}}}))
        .prompt()
        .unwrap();
        assert_eq!(p.schema, Some(json!({"type": "object"})));
        let p = req(
            json!({"model": "x", "messages": [{"role": "user", "content": "hi"}],
            "response_format": {"type": "json_object"}}),
        )
        .prompt()
        .unwrap();
        assert!(p.schema.is_none());
        assert!(p.system.unwrap().contains("single JSON object"));
    }

    #[test]
    fn image_parts_and_unknown_roles_are_refused() {
        assert!(req(json!({"model": "x", "messages": [
            {"role": "user", "content": [{"type": "image_url", "image_url": {"url": "x"}}]}]}))
        .prompt()
        .is_err());
        assert!(
            req(json!({"model": "x", "messages": [{"role": "tool", "content": "x"}]}))
                .prompt()
                .is_err()
        );
        assert!(req(json!({"model": "x", "messages": []})).prompt().is_err());
    }
}
