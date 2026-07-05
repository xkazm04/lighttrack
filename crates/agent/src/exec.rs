//! Execute one leased task: resolve its action, render the prompt, run the Claude Code CLI, and
//! propagate the result through the action's connector. Never panics — every path folds into a
//! `RunReport` the caller settles back to the cloud.

use anyhow::Result;
use serde_json::{json, Value};

use lighttrack_core::RelayTask;
use lighttrack_engine::{run_raw, EngineConfig, EngineError};

use crate::actions;
use crate::config::AgentConfig;
use crate::connect;

/// What the device reports back on settle (mirrors the result endpoint's body).
pub(crate) struct RunReport {
    /// `succeeded` | `failed` | `deferred`.
    pub status: &'static str,
    pub result: Value,
    pub error: Option<String>,
    pub retry_after_secs: Option<u32>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub latency_ms: Option<u64>,
}

impl RunReport {
    fn failed(error: String) -> Self {
        RunReport {
            status: "failed",
            result: Value::Null,
            error: Some(error),
            retry_after_secs: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            latency_ms: None,
        }
    }

    fn deferred(reason: String) -> Self {
        RunReport { status: "deferred", ..Self::failed(reason) }
    }
}

pub(crate) fn execute(cfg: &AgentConfig, engine: &EngineConfig, task: &RelayTask) -> RunReport {
    let action = match actions::load(&cfg.actions_dir, &task.action_type) {
        Ok(a) => a,
        // A missing/broken action is a real failure: retrying later is right (the user can add
        // the action to the library between attempts), and exhaustion dead-letters it.
        Err(e) => return RunReport::failed(format!("action: {e:#}")),
    };
    let prompt = actions::render(&action.prompt_template, task);

    let out = match run_raw(
        engine,
        &prompt,
        &action.spec.model,
        action.spec.system.as_deref(),
        action.schema.as_deref(),
    ) {
        Ok(out) => out,
        Err(e) if rate_limited(&e) => return RunReport::deferred(format!("claude: {e}")),
        Err(e) => return RunReport::failed(format!("claude: {e}")),
    };

    // With a schema the result is the structured JSON itself; otherwise the raw text, wrapped.
    let result = match &action.schema {
        Some(_) => match serde_json::from_str::<Value>(&out.text) {
            Ok(v) => v,
            Err(e) => return RunReport::failed(format!("schema output is not JSON ({e}): {}", out.text)),
        },
        None => json!({ "text": out.text }),
    };

    if let Some(spec) = &action.spec.connector {
        if let Err(e) = deliver(spec, task, &result, &out.model) {
            // The Claude run itself succeeded, but the app never saw the result — that's a failed
            // attempt. The retry re-runs the action, which is why connectors must be idempotent.
            return RunReport::failed(format!("connector: {e:#}"));
        }
    }

    RunReport {
        status: "succeeded",
        result,
        error: None,
        retry_after_secs: None,
        model: Some(out.model),
        input_tokens: out.input_tokens,
        output_tokens: out.output_tokens,
        latency_ms: out.latency_ms,
    }
}

fn deliver(
    spec: &crate::connect::ConnectorSpec,
    task: &RelayTask,
    result: &Value,
    model: &str,
) -> Result<()> {
    connect::deliver(
        spec,
        &json!({
            "task_id": task.id,
            "action_type": task.action_type,
            "idempotency_key": task.idempotency_key,
            "source": task.source,
            "params": task.payload,
            "result": result,
            "model": model,
        }),
    )
}

/// Subscription-window / rate-limit errors must settle `deferred` (the attempt is handed back)
/// rather than burn one of the task's real retries.
fn rate_limited(e: &EngineError) -> bool {
    if let EngineError::NonZero { stderr, .. } = e {
        let s = stderr.to_lowercase();
        return s.contains("usage limit") || s.contains("rate limit") || s.contains("429") || s.contains("overloaded");
    }
    false
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn task(action_type: &str) -> RelayTask {
        serde_json::from_value(json!({
            "id": "t-1", "project_id": "p1", "action_type": action_type,
            "payload": { "who": "world" }
        }))
        .unwrap()
    }

    fn cfg(actions_dir: &str) -> AgentConfig {
        std::env::set_var("LT_TEST_DEVICE_KEY", "k");
        let toml = format!(
            "actions_dir = \"{}\"\n[[sources]]\nname = \"x\"\nurl = \"http://x\"\ndevice_key_env = \"LT_TEST_DEVICE_KEY\"\n",
            actions_dir.replace('\\', "\\\\")
        );
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn unknown_or_invalid_action_fails_without_invoking_claude() {
        let dir = tempfile::tempdir().unwrap();
        let engine = EngineConfig::default();
        let cfg = cfg(dir.path().to_str().unwrap());

        let r = execute(&cfg, &engine, &task("ns/missing"));
        assert_eq!(r.status, "failed");
        assert!(r.error.unwrap().contains("no action"));

        let r = execute(&cfg, &engine, &task("../escape"));
        assert_eq!(r.status, "failed");
        assert!(r.error.unwrap().contains("invalid action_type"));
    }

    #[test]
    fn rate_limit_stderr_classifies_as_deferred() {
        let rl = EngineError::NonZero { code: 1, stderr: "Claude AI usage limit reached|123".into() };
        assert!(rate_limited(&rl));
        let other = EngineError::NonZero { code: 1, stderr: "boom".into() };
        assert!(!rate_limited(&other));
        assert!(!rate_limited(&EngineError::Parse("x".into())));
    }
}
