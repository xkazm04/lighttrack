//! Execute one leased task: resolve its action, render the prompt, run the Claude Code CLI, and
//! propagate the result through the action's connector. Never panics — every path folds into a
//! `RunReport` the caller settles back to the cloud.

use anyhow::Result;
use serde_json::{json, Value};

use lighttrack_core::RelayTask;
use lighttrack_engine::invocation::{self, Invocation};
use lighttrack_engine::{EngineConfig, EngineError};

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
    /// What the CLI envelope said this run cost. The device reports it; the cloud still prices the
    /// relay event its own way (docs/RELAY.md), so this is evidence, not a bill.
    pub cost_usd: Option<f64>,
    /// The posture the run actually executed under — the cloud only ever named an `action_type`,
    /// so without this the settle record cannot say whether a repository was touched.
    pub mode: Option<&'static str>,
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
            cost_usd: None,
            mode: None,
        }
    }

    fn deferred(reason: String) -> Self {
        RunReport {
            status: "deferred",
            ..Self::failed(reason)
        }
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
    let spec = &action.spec;

    // Resolve the posture before spending anything: an action that claims a mode it cannot back up
    // (an edit run with no workspace, a scan naming a directory outside the root) fails here, at no
    // cost, instead of after a paid run.
    let workspace = match actions::resolve_workspace(
        cfg.workspaces_root.as_deref(),
        spec.workspace.as_deref(),
        spec.mode,
    ) {
        Ok(w) => w,
        Err(e) => return RunReport::failed(format!("action posture: {e:#}")),
    };
    let mut inv = Invocation::with_mode(&prompt, &spec.model, spec.mode)
        .with_system(spec.system.as_deref())
        .with_schema(action.schema.as_deref())
        .with_allowed_tools(spec.allowed_tools.clone())
        .with_permission_mode(spec.permission_mode.as_deref())
        .with_budget_usd(spec.max_budget_usd)
        .with_timeout(spec.timeout())
        .with_bare(engine.bare);
    if let Some(dir) = workspace {
        inv = inv.with_cwd(dir);
    }

    let out = match invocation::run(&engine.claude(), &inv) {
        Ok(out) => out,
        Err(e) if rate_limited(&e) => return RunReport::deferred(format!("claude: {e}")),
        Err(e) => return RunReport::failed(format!("claude: {e}")),
    };
    if !out.ok() {
        // An agentic mode reads its envelope even on a controlled non-zero exit (a budget cap), so
        // a run that reported an error must not settle `succeeded` with the error text as a result.
        return RunReport::failed(format!(
            "claude reported an error (subtype={}): {}",
            out.subtype,
            if out.text.is_empty() {
                out.stderr.as_str()
            } else {
                out.text.as_str()
            }
        ));
    }

    // With a schema the result is the structured JSON itself; otherwise the raw text, wrapped.
    let result = match &action.schema {
        Some(_) => match serde_json::from_str::<Value>(&out.text) {
            Ok(v) => v,
            Err(e) => {
                return RunReport::failed(format!("schema output is not JSON ({e}): {}", out.text))
            }
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
        cost_usd: out.cost_usd,
        mode: Some(spec.mode.as_str()),
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
        return s.contains("usage limit")
            || s.contains("rate limit")
            || s.contains("429")
            || s.contains("overloaded");
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
        cfg_with_root(actions_dir, None)
    }

    fn cfg_with_root(actions_dir: &str, workspaces_root: Option<&str>) -> AgentConfig {
        std::env::set_var("LT_TEST_DEVICE_KEY", "k");
        let esc = |s: &str| s.replace('\\', "\\\\");
        let root = workspaces_root
            .map(|r| format!("workspaces_root = \"{}\"\n", esc(r)))
            .unwrap_or_default();
        let toml = format!(
            "actions_dir = \"{}\"\n{root}[[sources]]\nname = \"x\"\nurl = \"http://x\"\ndevice_key_env = \"LT_TEST_DEVICE_KEY\"\n",
            esc(actions_dir)
        );
        toml::from_str(&toml).unwrap()
    }

    /// Write a one-file action library entry with the given `action.toml` body.
    fn write_action(dir: &std::path::Path, spec: &str) {
        let act = dir.join("ns").join("act");
        std::fs::create_dir_all(&act).unwrap();
        std::fs::write(act.join("prompt.md"), "Hello {{params.who}}").unwrap();
        std::fs::write(act.join("action.toml"), spec).unwrap();
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

    /// The posture an action declares is resolved before anything is spawned, so a library that
    /// over-claims costs nothing. `claude_bin` here cannot exist — reaching a spawn would be a
    /// different error than the one each case asserts.
    #[test]
    fn an_over_claiming_action_fails_before_any_spawn() {
        let engine = EngineConfig {
            claude_bin: "definitely-not-an-executable-lighttrack-test".into(),
            ..EngineConfig::default()
        };
        let cases: [(&str, bool, &str); 5] = [
            // (action.toml, configure a workspaces_root, expected substring)
            ("mode = \"edit\"\n", true, "requires a workspace"),
            (
                "mode = \"readonly-scan\"\nworkspace = \"repo\"\n",
                false,
                "no workspaces_root",
            ),
            (
                "mode = \"readonly-scan\"\nworkspace = \"../outside\"\n",
                true,
                "invalid workspace",
            ),
            (
                "mode = \"generate\"\nworkspace = \"repo\"\n",
                true,
                "takes no workspace",
            ),
            (
                // A repository-touching mode with a write tool: the seam refuses the posture.
                "mode = \"readonly-scan\"\nworkspace = \"repo\"\nallowed_tools = [\"Write\"]\n",
                true,
                "it can write",
            ),
        ];
        for (spec, with_root, expect) in cases {
            let lib = tempfile::tempdir().unwrap();
            let roots = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(roots.path().join("repo")).unwrap();
            write_action(lib.path(), spec);
            let cfg = cfg_with_root(
                lib.path().to_str().unwrap(),
                with_root.then(|| roots.path().to_str().unwrap()),
            );
            let r = execute(&cfg, &engine, &task("ns/act"));
            assert_eq!(r.status, "failed", "{spec}");
            let err = r.error.unwrap_or_default();
            assert!(
                err.contains(expect),
                "{spec}\nexpected '{expect}' in: {err}"
            );
            assert!(
                !err.contains("lighttrack-test"),
                "{spec}: reached a spawn instead of failing on posture: {err}"
            );
        }
    }

    /// A well-formed edit action passes posture and only then fails on the missing binary — the
    /// proof that the posture gate is not simply rejecting everything.
    #[test]
    fn a_well_formed_edit_action_reaches_the_spawn() {
        let lib = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(roots.path().join("repo")).unwrap();
        write_action(
            lib.path(),
            "mode = \"edit\"\nworkspace = \"repo\"\npermission_mode = \"acceptEdits\"\nmax_budget_usd = 2.0\ntimeout_secs = 30\n",
        );
        let engine = EngineConfig {
            claude_bin: "definitely-not-an-executable-lighttrack-test".into(),
            ..EngineConfig::default()
        };
        let cfg = cfg_with_root(
            lib.path().to_str().unwrap(),
            Some(roots.path().to_str().unwrap()),
        );
        let r = execute(&cfg, &engine, &task("ns/act"));
        assert_eq!(r.status, "failed");
        assert!(
            r.error.unwrap().contains("lighttrack-test"),
            "a valid posture should get as far as the spawn"
        );
    }

    #[test]
    fn rate_limit_stderr_classifies_as_deferred() {
        let rl = EngineError::NonZero {
            code: 1,
            stderr: "Claude AI usage limit reached|123".into(),
        };
        assert!(rate_limited(&rl));
        let other = EngineError::NonZero {
            code: 1,
            stderr: "boom".into(),
        };
        assert!(!rate_limited(&other));
        assert!(!rate_limited(&EngineError::Parse("x".into())));
    }
}
