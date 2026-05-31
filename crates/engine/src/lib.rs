//! The scoring engine: run `claude -p` as an LLM-as-judge and parse a [`JudgeVerdict`].
//!
//! Invocation:
//! ```text
//! claude -p "<prompt>" --output-format json --model <model> --json-schema '<JudgeVerdict schema>'
//! ```
//! We read `total_cost_usd` from the JSON envelope and the verdict from `structured_output`
//! (falling back to extracting a JSON object from the `result` text if a build doesn't return
//! `structured_output`). The judge is **unbudgeted** by design — callers never rate-limit it.

use std::process::{Command, Stdio};

use serde_json::Value;
use thiserror::Error;

use lighttrack_core::{judge_verdict_schema, JudgeVerdict};

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("failed to spawn '{bin}': {source}")]
    Spawn {
        bin: String,
        source: std::io::Error,
    },
    #[error("claude exited with status {code}: {stderr}")]
    NonZero { code: i32, stderr: String },
    #[error("could not parse judge output: {0}")]
    Parse(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// How to invoke the judge.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub claude_bin: String,
    pub model: String,
    /// Pass `--bare` to skip auto-loading hooks/skills/MCP/CLAUDE.md. This avoids paying to
    /// re-cache ~40k tokens of context on every call (a one-word reply otherwise cost ~$0.05),
    /// but `--bare` bypasses subscription OAuth, so it requires `ANTHROPIC_API_KEY` in the env.
    pub bare: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            claude_bin: "claude".to_string(),
            model: "haiku".to_string(),
            bare: false,
        }
    }
}

/// The result of one judge call.
#[derive(Debug, Clone)]
pub struct JudgeOutcome {
    pub verdict: JudgeVerdict,
    pub cost_usd: Option<f64>,
    pub model: String,
    pub session_id: Option<String>,
}

/// Build a judging prompt for an input/output pair against a rubric.
pub fn build_judge_prompt(rubric: &str, input: &str, output: &str) -> String {
    format!(
        "You are a strict evaluation judge. Evaluate the ASSISTANT OUTPUT for the given USER INPUT \
against the rubric below.\n\
Rubric: {rubric}\n\n\
Respond with ONLY a JSON object (no prose, no code fences) of the form:\n\
{{\"score\": <number 0.0-1.0>, \"max\": 1.0, \"pass\": <true|false>, \"reasoning\": \"<one sentence>\"}}\n\n\
=== USER INPUT ===\n{input}\n\n=== ASSISTANT OUTPUT ===\n{output}\n"
    )
}

/// Run the judge with a fully-formed prompt.
pub fn run_judge(cfg: &EngineConfig, prompt: &str) -> Result<JudgeOutcome> {
    let schema = judge_verdict_schema().to_string();
    let mut cmd = Command::new(&cfg.claude_bin);
    cmd.arg("-p")
        .arg(prompt)
        .arg("--output-format")
        .arg("json")
        .arg("--model")
        .arg(&cfg.model)
        .arg("--json-schema")
        .arg(&schema)
        .stdin(Stdio::null()); // don't block waiting for piped stdin
    if cfg.bare {
        cmd.arg("--bare");
    }
    let output = cmd.output().map_err(|source| EngineError::Spawn {
        bin: cfg.claude_bin.clone(),
        source,
    })?;

    if !output.status.success() {
        return Err(EngineError::NonZero {
            code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    let envelope: Value = serde_json::from_slice(&output.stdout).map_err(|e| {
        EngineError::Parse(format!(
            "envelope not JSON: {e}; stdout was: {}",
            String::from_utf8_lossy(&output.stdout)
        ))
    })?;

    let verdict = parse_verdict(&envelope)?;
    let cost_usd = envelope.get("total_cost_usd").and_then(Value::as_f64);
    let session_id = envelope
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let model = envelope
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| cfg.model.clone());

    Ok(JudgeOutcome {
        verdict,
        cost_usd,
        model,
        session_id,
    })
}

/// Prefer `structured_output`; otherwise extract a JSON object from the `result` text.
fn parse_verdict(envelope: &Value) -> Result<JudgeVerdict> {
    if let Some(s) = envelope.get("structured_output") {
        if !s.is_null() {
            return Ok(serde_json::from_value(s.clone())?);
        }
    }
    let result = envelope
        .get("result")
        .and_then(Value::as_str)
        .ok_or_else(|| EngineError::Parse("no structured_output and no result text".into()))?;
    let json = extract_json_object(result)
        .ok_or_else(|| EngineError::Parse(format!("no JSON object in result: {result}")))?;
    serde_json::from_str(&json)
        .map_err(|e| EngineError::Parse(format!("result JSON not a verdict: {e}; got: {json}")))
}

/// Extract the outermost `{...}` from a string (handles stray prose / code fences).
fn extract_json_object(s: &str) -> Option<String> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end > start {
        Some(s[start..=end].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_structured_output() {
        let env = serde_json::json!({
            "total_cost_usd": 0.0004,
            "structured_output": {"score": 0.9, "max": 1.0, "pass": true, "reasoning": "good"}
        });
        let v = parse_verdict(&env).unwrap();
        assert_eq!(v.score, 0.9);
        assert!(v.pass);
    }

    #[test]
    fn falls_back_to_result_text() {
        let env = serde_json::json!({
            "result": "Here is my verdict:\n```json\n{\"score\":0.2,\"max\":1.0,\"pass\":false,\"reasoning\":\"wrong\"}\n```"
        });
        let v = parse_verdict(&env).unwrap();
        assert_eq!(v.score, 0.2);
        assert!(!v.pass);
    }

    #[test]
    fn extracts_object() {
        assert_eq!(extract_json_object("noise {\"a\":1} tail"), Some("{\"a\":1}".to_string()));
        assert_eq!(extract_json_object("no json here"), None);
    }
}
