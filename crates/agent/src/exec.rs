//! **Produce** the result of one leased task: resolve its action, render the prompt, run the Claude
//! Code CLI, and hand back the delivery it has not made. Never panics — every path folds into a
//! `RunReport` the caller settles back to the cloud.
//!
//! Delivery deliberately is NOT here (`crate::deliver`). A connector call is the one effect the
//! cloud cannot take back, so the caller's lease check has to sit between producing the result and
//! propagating it; while the call lived on this module's success path there was no such seam, and a
//! cancelled run delivered before anybody could ask whether it still should.

use lighttrack_core::RelayTask;
use lighttrack_engine::invocation::{self, Invocation};
use lighttrack_engine::{EngineConfig, EngineError};
use serde_json::{json, Value};

use crate::actions;
use crate::config::AgentConfig;
use crate::deliver::{Pending, Produced};
use crate::progress::{self, Emitter, Phase};
use crate::report::{PromptIdentity, RunReport};

/// `px` is write-only telemetry (`crate::progress`): emitting a phase cannot block, cannot fail and
/// returns nothing, so no path through this module can be changed by whether the cloud heard it.
pub(crate) fn execute(
    cfg: &AgentConfig,
    engine: &EngineConfig,
    task: &RelayTask,
    px: &Emitter,
) -> Produced {
    let action = match actions::load(&cfg.actions_dir, &task.action_type) {
        Ok(a) => a,
        // A missing/broken action is a real failure: retrying later is right (the user can add
        // the action to the library between attempts), and exhaustion dead-letters it.
        // Unstamped, and the only outcome that is: there was no prompt to fingerprint.
        Err(e) => return unstamped(format!("action: {e:#}")),
    };
    px.emit(Phase::Resolving {
        action: task.action_type.clone(),
        version: action.spec.version.clone(),
    });
    // Which prompt text is about to run, computed before anything is spawned so that every other
    // outcome — including a posture refusal that costs nothing — can name it.
    let prompt = match actions::render(&action.prompt_template, task) {
        Ok(p) => p,
        // Nothing was spawned; like a broken action this is retryable (the payload or the template
        // can be fixed between attempts) and unstamped: there is no complete prompt to fingerprint.
        Err(e) => return unstamped(format!("prompt: {e:#}")),
    };
    let identity = PromptIdentity::new(
        &prompt,
        action.spec.version.as_deref(),
        action.spec.report_io,
    );
    let (report, delivery) = run(cfg, engine, task, &action, &prompt, &identity, px);
    Produced {
        report: report.stamp(&identity),
        identity: Some(identity),
        delivery,
    }
}

/// A failure that never reached a prompt: nothing to fingerprint, and nothing to deliver.
fn unstamped(error: String) -> Produced {
    Produced {
        report: RunReport::failed(error),
        identity: None,
        delivery: None,
    }
}

/// The report, plus the connector call the caller must decide about. A `None` delivery is either
/// "the run failed" or "the action declares no connector" — in both cases there is nothing to hold
/// back.
#[allow(clippy::too_many_arguments)]
fn run(
    cfg: &AgentConfig,
    engine: &EngineConfig,
    task: &RelayTask,
    action: &actions::Action,
    prompt: &str,
    identity: &PromptIdentity,
    px: &Emitter,
) -> (RunReport, Option<Pending>) {
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
        Err(e) => return (RunReport::failed(format!("action posture: {e:#}")), None),
    };
    let mut inv = Invocation::with_mode(prompt, &spec.model, spec.mode)
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

    // The span that can last hours and published nothing at all. The phase goes out before it,
    // because the whole point is to be readable from the cloud *while* it is in flight.
    let connector = progress::connector_kind(spec.connector.as_ref());
    px.emit(Phase::Spawned {
        mode: spec.mode.as_str(),
        // The declared NAME, never the resolved path: the operator needs to know which repository,
        // not where this device keeps it.
        workspace: spec.workspace.clone(),
        model: spec.model.clone(),
        fingerprint: identity.prompt_sha256.clone(),
    });
    let out = match invocation::run(&engine.claude(), &inv) {
        Ok(out) => out,
        Err(e) if rate_limited(&e) => {
            px.emit(Phase::Exited {
                subtype: "rate-limited".to_string(),
                connector,
            });
            return (RunReport::deferred(format!("claude: {e}")), None);
        }
        Err(e) => {
            px.emit(Phase::Exited {
                subtype: "error".to_string(),
                connector,
            });
            return (RunReport::failed(format!("claude: {e}")), None);
        }
    };
    px.emit(Phase::Exited {
        subtype: out.subtype.clone(),
        connector,
    });
    if !out.ok() {
        // An agentic mode reads its envelope even on a controlled non-zero exit (a budget cap), so
        // a run that reported an error must not settle `succeeded` with the error text as a result.
        return (
            RunReport::failed(format!(
                "claude reported an error (subtype={}): {}",
                out.subtype,
                if out.text.is_empty() {
                    out.stderr.as_str()
                } else {
                    out.text.as_str()
                }
            )),
            None,
        );
    }

    // With a schema the result is the structured JSON itself; otherwise the raw text, wrapped.
    let result = match &action.schema {
        Some(_) => match serde_json::from_str::<Value>(&out.text) {
            Ok(v) => v,
            Err(e) => {
                return (
                    RunReport::failed(format!("schema output is not JSON ({e}): {}", out.text)),
                    None,
                )
            }
        },
        None => json!({ "text": out.text }),
    };

    // Prepared, not sent. The caller checks the lease between here and `deliver::propagate`.
    let delivery = action
        .spec
        .connector
        .as_ref()
        .map(|spec| Pending::new(spec, task, &result, &out.model));

    (
        RunReport {
            status: "succeeded",
            // Opt-in, and the model's raw text rather than the `{"text": …}` wrapper: what a judge
            // has to read is what the model wrote.
            result_text: spec.report_io.then(|| out.text.clone()),
            result,
            error: None,
            model: Some(out.model),
            input_tokens: out.input_tokens,
            output_tokens: out.output_tokens,
            latency_ms: out.latency_ms,
            cost_usd: out.cost_usd,
            mode: Some(spec.mode.as_str()),
            ..RunReport::failed(String::new())
        },
        delivery,
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
mod tests;
