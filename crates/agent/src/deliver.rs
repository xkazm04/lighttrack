//! The irreversible half of a run: pushing the result into the originating app.
//!
//! It lives here, and not in `exec`, for one reason. A connector call is the one thing the cloud
//! cannot take back, so the lease check has to sit *between* producing the result and delivering
//! it — and while the call was buried inside `exec`'s success path, no check could: the only stop
//! flag the agent had was consulted after `exec` returned, which is after delivery. An operator who
//! cancelled a repository-editing run still got the connector write, and the task then reported
//! `cancelled`.
//!
//! So `exec` now *produces* — it runs the CLI and builds the report — and hands back the delivery
//! it has not made. Nothing between the two is reachable without passing the caller's check.

use serde_json::{json, Value};

use lighttrack_core::RelayTask;

use crate::connect::{self, ConnectorSpec};
use crate::report::{PromptIdentity, RunReport};

/// A result produced but not yet propagated.
pub(crate) struct Produced {
    /// What the run would settle, before delivery has had its say.
    pub report: RunReport,
    /// The run's identity, kept so a delivery failure's report still names the prompt that ran.
    /// `None` only for the outcomes that never reached a prompt — which also never deliver.
    pub identity: Option<PromptIdentity>,
    /// `Some` only when the run succeeded *and* the action declares a connector: the one effect
    /// that cannot be undone, deliberately held back.
    pub delivery: Option<Pending>,
}

/// One connector call, prepared and not yet made.
pub(crate) struct Pending {
    spec: ConnectorSpec,
    envelope: Value,
}

impl Pending {
    pub(crate) fn new(spec: &ConnectorSpec, task: &RelayTask, result: &Value, model: &str) -> Self {
        Pending {
            spec: spec.clone(),
            envelope: json!({
                "task_id": task.id,
                "action_type": task.action_type,
                "idempotency_key": task.idempotency_key,
                "source": task.source,
                "params": task.payload,
                "result": result,
                "model": model,
            }),
        }
    }
}

/// Deliver, and fold a connector failure into the report the caller settles.
///
/// The Claude run itself succeeded, but the app never saw the result — that is a failed attempt.
/// The retry re-runs the action, which is why connectors must be idempotent.
pub(crate) fn propagate(produced: Produced) -> RunReport {
    let Produced {
        report,
        identity,
        delivery,
    } = produced;
    let Some(pending) = delivery else {
        return report;
    };
    match connect::deliver(&pending.spec, &pending.envelope) {
        Ok(()) => report,
        Err(e) => {
            let failed = RunReport::failed(format!("connector: {e:#}"));
            match &identity {
                Some(id) => failed.stamp(id),
                None => failed,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> RelayTask {
        serde_json::from_value(json!({
            "id": "t-1", "project_id": "p1", "action_type": "ns/act",
            "payload": { "who": "world" }
        }))
        .expect("task")
    }

    /// A `command` connector that proves it ran by leaving a file behind — the closest thing to an
    /// irreversible side effect a test can own, and the thing a cancelled run must not produce.
    /// `mkdir` rather than a shell redirect: the side effect is the process's own, with no
    /// metacharacter for a platform to disagree about.
    fn marking_connector(marker: &std::path::Path) -> ConnectorSpec {
        let path = marker.to_str().expect("utf-8 path").to_string();
        let command: Vec<String> = if cfg!(windows) {
            vec!["cmd".into(), "/C".into(), "mkdir".into(), path]
        } else {
            vec!["mkdir".into(), path]
        };
        serde_json::from_value(json!({ "kind": "command", "command": command })).expect("spec")
    }

    fn produced(dir: &std::path::Path, delivered: bool) -> Produced {
        let spec = marking_connector(&dir.join("delivered"));
        let identity = PromptIdentity::new("Hello world", Some("7"), false);
        Produced {
            report: RunReport {
                status: "succeeded",
                ..RunReport::failed(String::new())
            }
            .stamp(&identity),
            identity: Some(PromptIdentity::new("Hello world", Some("7"), false)),
            delivery: delivered.then(|| Pending::new(&spec, &task(), &json!({"n": 1}), "m")),
        }
    }

    /// The point of the whole card: delivery happens when — and only when — the caller asks for it.
    /// A cancelled run keeps its produced report and makes no connector call at all.
    #[test]
    fn a_result_reaches_the_app_only_through_the_call_the_caller_makes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("delivered");

        // The cancel path: the Produced is simply never propagated.
        let p = produced(dir.path(), true);
        assert!(p.delivery.is_some(), "the action declares a connector");
        drop(p);
        assert!(
            !marker.exists(),
            "a result nobody propagated must not have reached the app"
        );

        // The healthy path, same action: propagate, and the connector fires.
        let report = propagate(produced(dir.path(), true));
        assert_eq!(report.status, "succeeded");
        assert!(
            marker.exists(),
            "the connector must fire when nothing stopped it"
        );
        assert_eq!(
            report.action_version.as_deref(),
            Some("7"),
            "the report keeps its identity through delivery"
        );
    }

    /// An action with no connector propagates to nothing and keeps its report untouched.
    #[test]
    fn an_action_with_no_connector_delivers_nothing_and_still_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = propagate(produced(dir.path(), false));
        assert_eq!(report.status, "succeeded");
        assert!(!dir.path().join("delivered").exists());
    }

    /// A connector failure is the run's outcome, and the report still names the prompt that ran —
    /// identically to how `exec` reported it before delivery moved out of it.
    #[test]
    fn a_connector_failure_becomes_the_runs_outcome_without_losing_its_identity() {
        let identity = PromptIdentity::new("Hello world", Some("7"), false);
        let spec: ConnectorSpec = serde_json::from_value(json!({
            "kind": "command",
            "command": ["definitely-not-an-executable-lighttrack-test"],
        }))
        .expect("spec");
        let report = propagate(Produced {
            report: RunReport {
                status: "succeeded",
                ..RunReport::failed(String::new())
            },
            delivery: Some(Pending::new(&spec, &task(), &json!({}), "m")),
            identity: Some(identity),
        });
        assert_eq!(report.status, "failed");
        assert!(report
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("connector"));
        assert_eq!(report.action_version.as_deref(), Some("7"));
    }

    /// The architectural half of the same claim, pinned where it can regress: `exec` must not be
    /// able to reach the connector at all. A check that lives only in the caller is a check the
    /// next refactor moves back inside the span it was put outside of.
    ///
    /// Comments are stripped before matching, so a mention of `connect::` in prose cannot make the
    /// guard report a clean file — the failure mode where a gate that cannot match is
    /// indistinguishable from success.
    #[test]
    fn exec_cannot_reach_the_connector() {
        let code = strip_comments(include_str!("exec.rs"));
        assert!(
            !code.contains("connect::"),
            "exec.rs reaches the connector itself; delivery must stay behind the lease check"
        );
        // Seeded, so the matcher is proved to be able to match at all.
        assert!(strip_comments("let x = connect::deliver(a);").contains("connect::"));
        assert!(!strip_comments("// calls connect::deliver one day").contains("connect::"));
    }

    fn strip_comments(src: &str) -> String {
        src.lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
    }
}
