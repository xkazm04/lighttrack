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
use std::time::Instant;

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
    /// Wall-clock latency of the `claude -p` call.
    pub latency_ms: Option<u64>,
    /// Total input tokens (prompt + cache read + cache creation), if reported.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// The result of a free-form text call (e.g. LLM-based anonymization).
#[derive(Debug, Clone)]
pub struct TextOutcome {
    pub text: String,
    pub cost_usd: Option<f64>,
    pub model: String,
    pub latency_ms: Option<u64>,
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

/// Build a benchmark eval prompt for an input/output pair, with an optional reference answer.
pub fn build_eval_prompt(rubric: &str, input: &str, expected: Option<&str>, output: &str) -> String {
    let reference = match expected {
        Some(e) => format!("\n=== REFERENCE / EXPECTED ANSWER ===\n{e}\n"),
        None => String::new(),
    };
    format!(
        "You are a strict evaluation judge. Evaluate the ASSISTANT OUTPUT for the given USER INPUT \
against the rubric{ref_note}.\n\
Rubric: {rubric}\n\n\
Respond with ONLY a JSON object (no prose, no code fences):\n\
{{\"score\": <number 0.0-1.0>, \"max\": 1.0, \"pass\": <true|false>, \"reasoning\": \"<one sentence>\"}}\n\n\
=== USER INPUT ===\n{input}\n{reference}\n=== ASSISTANT OUTPUT ===\n{output}\n",
        ref_note = if expected.is_some() {
            " and the reference answer"
        } else {
            ""
        }
    )
}

/// Run `claude -p` with the given prompt, returning the parsed JSON envelope and wall-clock latency.
fn invoke(cfg: &EngineConfig, prompt: &str, schema: Option<&str>) -> Result<(Value, Option<u64>)> {
    let mut cmd = Command::new(&cfg.claude_bin);
    cmd.arg("-p")
        .arg(prompt)
        .arg("--output-format")
        .arg("json")
        .arg("--model")
        .arg(&cfg.model)
        .stdin(Stdio::null()); // don't block waiting for piped stdin
    if let Some(s) = schema {
        cmd.arg("--json-schema").arg(s);
    }
    if cfg.bare {
        cmd.arg("--bare");
    }
    let started = Instant::now();
    let output = cmd.output().map_err(|source| EngineError::Spawn {
        bin: cfg.claude_bin.clone(),
        source,
    })?;
    let latency_ms = Some(started.elapsed().as_millis() as u64);

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
    Ok((envelope, latency_ms))
}

fn token_counts(envelope: &Value) -> (Option<u64>, Option<u64>) {
    let usage = envelope.get("usage");
    let input = usage.map(|u| {
        let f = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        f("input_tokens") + f("cache_read_input_tokens") + f("cache_creation_input_tokens")
    });
    let output = usage.and_then(|u| u.get("output_tokens").and_then(Value::as_u64));
    (input, output)
}

fn model_of(envelope: &Value, cfg: &EngineConfig) -> String {
    envelope
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| cfg.model.clone())
}

/// Run the judge with a fully-formed prompt.
pub fn run_judge(cfg: &EngineConfig, prompt: &str) -> Result<JudgeOutcome> {
    let schema = judge_verdict_schema().to_string();
    let (envelope, latency_ms) = invoke(cfg, prompt, Some(&schema))?;
    let (input_tokens, output_tokens) = token_counts(&envelope);
    Ok(JudgeOutcome {
        verdict: parse_verdict(&envelope)?,
        cost_usd: envelope.get("total_cost_usd").and_then(Value::as_f64),
        model: model_of(&envelope, cfg),
        session_id: envelope
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        latency_ms,
        input_tokens,
        output_tokens,
    })
}

/// Run a free-form text generation (no schema), returning the `result` text. Used by the optional
/// LLM anonymization pass.
pub fn run_text(cfg: &EngineConfig, prompt: &str) -> Result<TextOutcome> {
    let (envelope, latency_ms) = invoke(cfg, prompt, None)?;
    Ok(TextOutcome {
        text: envelope
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        cost_usd: envelope.get("total_cost_usd").and_then(Value::as_f64),
        model: model_of(&envelope, cfg),
        latency_ms,
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
