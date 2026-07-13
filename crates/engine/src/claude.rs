//! The `claude -p` subprocess caller and shared envelope helpers.

use std::process::{Command, Stdio};
use std::time::Instant;

use serde_json::Value;

use crate::{EngineConfig, EngineError, Result};

/// Run `claude -p` with the given prompt/model, returning the parsed JSON envelope and latency.
pub(crate) fn invoke(
    cfg: &EngineConfig,
    prompt: &str,
    model: &str,
    system_prompt: Option<&str>,
    schema: Option<&str>,
) -> Result<(Value, Option<u64>)> {
    // A trailing `@<effort>` on the model selects the CLI reasoning effort
    // (low|medium|high|xhigh|max), e.g. "opus@xhigh" — lets the judge reason deeply while candidate
    // generations stay at their default. No suffix ⇒ the model runs as-is.
    let (model, effort) = split_effort(model);
    let mut cmd = Command::new(&cfg.claude_bin);
    cmd.arg("-p")
        .arg(prompt)
        .arg("--output-format")
        .arg("json")
        .arg("--model")
        .arg(model)
        .stdin(Stdio::null()); // don't block waiting for piped stdin
    if let Some(level) = effort {
        cmd.arg("--effort").arg(level);
    }
    if let Some(sys) = system_prompt {
        cmd.arg("--append-system-prompt").arg(sys);
    }
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

/// The completion text from a claude envelope. With `--json-schema` the model's structured answer
/// lands in `structured_output` (an object) — prefer it, serialized, so downstream JSON extraction
/// sees clean JSON; otherwise fall back to the free-text `result`.
pub(crate) fn completion_text(envelope: &Value) -> String {
    if let Some(v) = envelope.get("structured_output").filter(|v| !v.is_null()) {
        return v.to_string();
    }
    envelope
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Total (input, output) tokens from a claude `usage` block (input includes cache read + creation).
pub(crate) fn token_counts(envelope: &Value) -> (Option<u64>, Option<u64>) {
    let usage = envelope.get("usage");
    let input = usage.map(|u| {
        let f = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        f("input_tokens") + f("cache_read_input_tokens") + f("cache_creation_input_tokens")
    });
    let output = usage.and_then(|u| u.get("output_tokens").and_then(Value::as_u64));
    (input, output)
}

/// Split a trailing `@<effort>` (low|medium|high|xhigh|max) off a model spec, e.g.
/// "opus@xhigh" → ("opus", Some("xhigh")). Any other string → (model, None).
fn split_effort(model: &str) -> (&str, Option<&str>) {
    if let Some((m, e)) = model.rsplit_once('@') {
        if matches!(e, "low" | "medium" | "high" | "xhigh" | "max") {
            return (m, Some(e));
        }
    }
    (model, None)
}

/// Resolve the model name reported in the envelope, falling back to `fallback`.
pub(crate) fn model_of(envelope: &Value, fallback: &str) -> String {
    envelope
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

/// The result of one raw `claude -p` call, for callers that build their own prompts
/// (e.g. `lt-agent`'s action library) and need the usage accounting alongside the text.
#[derive(Debug, Clone)]
pub struct RawOutcome {
    pub text: String,
    pub model: String,
    pub cost_usd: Option<f64>,
    pub latency_ms: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// Run one raw `claude -p` call: optional system prompt, optional `--json-schema` (the returned
/// `text` is then the schema-conforming JSON), `@<effort>` model suffixes as everywhere else.
pub fn run_raw(
    cfg: &EngineConfig,
    prompt: &str,
    model: &str,
    system_prompt: Option<&str>,
    schema: Option<&str>,
) -> Result<RawOutcome> {
    let (envelope, latency_ms) = invoke(cfg, prompt, model, system_prompt, schema)?;
    let (input_tokens, output_tokens) = token_counts(&envelope);
    Ok(RawOutcome {
        text: envelope
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        model: model_of(&envelope, model),
        cost_usd: envelope.get("total_cost_usd").and_then(Value::as_f64),
        latency_ms,
        input_tokens,
        output_tokens,
    })
}

/// Resolve a runnable claude executable. A child process can't invoke the npm `.cmd`/`.ps1` shims
/// with our quote-heavy args, so on Windows we prefer the real `claude.exe` the shim wraps.
pub fn resolve_claude_bin(given: &str) -> String {
    if given != "claude" {
        return given.to_string();
    }
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            let p =
                format!("{appdata}\\npm\\node_modules\\@anthropic-ai\\claude-code\\bin\\claude.exe");
            if std::path::Path::new(&p).exists() {
                return p;
            }
        }
    }
    given.to_string()
}

#[cfg(test)]
mod tests {
    use super::resolve_claude_bin;

    #[test]
    fn resolve_claude_bin_passes_through_explicit_paths() {
        assert_eq!(resolve_claude_bin("/usr/bin/claude"), "/usr/bin/claude");
        assert_eq!(resolve_claude_bin("C:\\tools\\claude.exe"), "C:\\tools\\claude.exe");
    }
}
