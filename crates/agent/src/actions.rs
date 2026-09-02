//! The local action library: `<actions_dir>/<action_type>/` holds `prompt.md` (a template with
//! `{{…}}` placeholders), `action.toml` (model + options + connector), and optionally
//! `schema.json`. The library is the device-side half of the relay contract — the cloud only ever
//! names an `action_type` and supplies params; everything executable lives here, gitignored.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use lighttrack_core::RelayTask;
use lighttrack_engine::invocation::{self, Mode};

use crate::connect::ConnectorSpec;

#[derive(Debug, Deserialize)]
pub(crate) struct ActionSpec {
    /// Model for this action, with the usual optional `@effort` suffix (e.g. `sonnet@high`).
    #[serde(default = "default_model")]
    pub model: String,
    /// Optional system prompt appended to the CLI call.
    #[serde(default)]
    pub system: Option<String>,
    /// Optional JSON-schema file (relative to the action dir); when set, the run's result is the
    /// schema-conforming JSON instead of free text.
    #[serde(default)]
    pub schema_file: Option<String>,
    /// What this action's run is allowed to be: `generate` (default — a completion with no tools
    /// and no repository), `readonly-scan`, or `edit`. `docs/RELAY.md` always promised that allowed
    /// tools live on the device; until the seam landed the library could not actually say so, and
    /// every action silently ran as a plain completion.
    #[serde(default)]
    pub mode: Mode,
    /// Workspace for a scan/edit run, relative to the agent's `workspaces_root`. Required by any
    /// mode other than `generate`, and resolved under that root — the cloud never names a path.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Extra tools beyond the mode's base allowlist. The seam rejects a write-capable entry on a
    /// `readonly-scan`.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// `--permission-mode`. Required by `edit`; `plan`/`default` are the legal values for a scan.
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Per-run spend ceiling handed to the CLI.
    #[serde(default)]
    pub max_budget_usd: Option<f64>,
    /// Wall-clock ceiling for this action's run; defaults to the engine's.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// A label the author bumps when they change `prompt.md`. Free text (`"3"`, `"2026-09-02"`,
    /// `"v2-tighter-rubric"`) — the cloud never parses it, it only groups by it beside the
    /// rendered prompt's fingerprint. Optional because the fingerprint is what actually detects a
    /// change; the version is what makes the change *legible* in a report.
    #[serde(default)]
    pub version: Option<String>,
    /// Send the rendered prompt and the result text to the cloud on settle, so the run can be
    /// judged like any other LLM call.
    ///
    /// **Off by default, and that default is the privacy contract.** An action's prompt and its
    /// result are the two things `docs/RELAY.md` promises never leave the device unasked; turning
    /// this on is the operator deciding, per action, that this particular workload's content is
    /// safe to store in their cloud instance. With it off the cloud still gets `prompt_sha256`, so
    /// a silent prompt regression is detectable without the text.
    #[serde(default)]
    pub report_io: bool,
    #[serde(default)]
    pub connector: Option<ConnectorSpec>,
}

impl ActionSpec {
    pub fn timeout(&self) -> Duration {
        self.timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(invocation::DEFAULT_TIMEOUT)
    }
}

fn default_model() -> String {
    "sonnet".to_string()
}

/// A fully-loaded action: spec + prompt template + resolved schema text.
pub(crate) struct Action {
    pub spec: ActionSpec,
    pub prompt_template: String,
    pub schema: Option<String>,
}

/// `action_type` comes from the network — constrain it to library-relative names
/// (`ns/action-name`) so it can never escape `actions_dir`.
pub(crate) fn validate_action_type(action_type: &str) -> Result<()> {
    let ok = !action_type.is_empty()
        && !action_type.starts_with('/')
        && !action_type.ends_with('/')
        && !action_type.contains("..")
        && action_type
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/'));
    if !ok {
        bail!("invalid action_type '{action_type}'");
    }
    Ok(())
}

/// Resolve an action's `workspace` under the agent's `workspaces_root`.
///
/// A scan or edit run needs a directory, and the only safe place for that decision is the device:
/// the cloud names an `action_type`, never a path. So the name is constrained exactly like
/// `action_type` (no absolute path, no `..`, no drive letters or backslashes), joined under a root
/// the operator opted into, and required to exist — an edit run that silently created its own
/// workspace would be an edit run nobody could review.
pub(crate) fn resolve_workspace(
    workspaces_root: Option<&Path>,
    workspace: Option<&str>,
    mode: Mode,
) -> Result<Option<PathBuf>> {
    if mode == Mode::Generate {
        // A completion has no repository by construction; naming one is a config mistake worth
        // saying out loud rather than ignoring.
        if workspace.is_some() {
            bail!("action mode 'generate' takes no workspace — set mode = \"readonly-scan\" or \"edit\"");
        }
        return Ok(None);
    }
    let name = workspace.ok_or_else(|| {
        anyhow::anyhow!("action mode '{mode}' requires a workspace (relative to workspaces_root)")
    })?;
    validate_action_type(name).with_context(|| format!("invalid workspace '{name}'"))?;
    let root = workspaces_root.ok_or_else(|| {
        anyhow::anyhow!(
            "action mode '{mode}' needs a workspace, but the agent config sets no workspaces_root"
        )
    })?;
    let path = root.join(name);
    if !path.is_dir() {
        bail!("workspace '{name}' does not exist under {}", root.display());
    }
    Ok(Some(path))
}

pub(crate) fn load(actions_dir: &str, action_type: &str) -> Result<Action> {
    validate_action_type(action_type)?;
    let dir: PathBuf = Path::new(actions_dir).join(action_type);
    if !dir.is_dir() {
        bail!(
            "no action '{action_type}' in library '{actions_dir}' (expected {})",
            dir.display()
        );
    }
    let spec_path = dir.join("action.toml");
    let spec: ActionSpec = if spec_path.exists() {
        toml::from_str(&std::fs::read_to_string(&spec_path)?)
            .with_context(|| format!("parsing {}", spec_path.display()))?
    } else {
        toml::from_str("").unwrap() // all-defaults action: just a prompt
    };
    let prompt_template = std::fs::read_to_string(dir.join("prompt.md"))
        .with_context(|| format!("action '{action_type}' has no prompt.md"))?;
    let schema = match &spec.schema_file {
        Some(f) => Some(
            std::fs::read_to_string(dir.join(f))
                .with_context(|| format!("action '{action_type}': schema file '{f}'"))?,
        ),
        None => None,
    };
    Ok(Action {
        spec,
        prompt_template,
        schema,
    })
}

/// Substitute `{{…}}` placeholders: `{{params.<dotted.path>}}` reads from the task payload
/// (strings verbatim, other values as JSON), `{{payload}}` is the whole payload as JSON, plus
/// `{{task_id}}` / `{{action_type}}`. Unknown placeholders render empty (a warning on stderr).
pub(crate) fn render(template: &str, task: &RelayTask) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let token = after[..end].trim();
        match resolve(token, task) {
            Some(v) => out.push_str(&v),
            None => eprintln!("warn: prompt placeholder '{{{{{token}}}}}' resolved empty"),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

fn resolve(token: &str, task: &RelayTask) -> Option<String> {
    match token {
        "task_id" => Some(task.id.clone()),
        "action_type" => Some(task.action_type.clone()),
        "payload" => serde_json::to_string(&task.payload).ok(),
        _ => {
            let path = token.strip_prefix("params.")?;
            let mut cur = &task.payload;
            for seg in path.split('.') {
                cur = cur.get(seg)?;
            }
            Some(match cur {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
        }
    }
}

/// Expand `${VAR}` references from the environment; used for connector header values so
/// credentials never live in the action files.
pub(crate) fn expand_env(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            bail!("unterminated ${{…}} in '{s}'");
        };
        let var = &after[..end];
        let val = std::env::var(var).with_context(|| format!("env var {var} is not set"))?;
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Header maps with env expansion applied to every value.
pub(crate) fn expand_headers(
    headers: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    headers
        .iter()
        .map(|(k, v)| Ok((k.clone(), expand_env(v)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn task(payload: Value) -> RelayTask {
        serde_json::from_value(json!({
            "id": "t-1", "project_id": "p1", "action_type": "ns/act", "payload": payload
        }))
        .unwrap()
    }

    #[test]
    fn render_substitutes_params_payload_and_ids() {
        let t = task(json!({ "sku": "A-1", "n": 3, "nest": { "deep": "x" } }));
        let got = render(
            "sku={{params.sku}} n={{params.n}} deep={{ params.nest.deep }} id={{task_id}} all={{payload}} missing={{params.nope}}!",
            &t,
        );
        assert_eq!(
            got,
            format!(
                "sku=A-1 n=3 deep=x id=t-1 all={} missing=!",
                serde_json::to_string(&t.payload).unwrap()
            )
        );
    }

    #[test]
    fn action_type_validation_blocks_traversal() {
        assert!(validate_action_type("xprice/reprice-summary").is_ok());
        assert!(validate_action_type("../../etc/passwd").is_err());
        assert!(validate_action_type("a\\b").is_err());
        assert!(validate_action_type("/abs").is_err());
        assert!(validate_action_type("a/../b").is_err());
        assert!(validate_action_type("").is_err());
    }

    #[test]
    fn load_reads_spec_prompt_and_schema() {
        let dir = tempfile::tempdir().unwrap();
        let act = dir.path().join("ns").join("act");
        std::fs::create_dir_all(&act).unwrap();
        std::fs::write(act.join("prompt.md"), "Hello {{params.who}}").unwrap();
        std::fs::write(act.join("action.toml"), "model = \"haiku\"\nschema_file = \"schema.json\"\n[connector]\nkind = \"http\"\nurl = \"https://x\"\n").unwrap();
        std::fs::write(act.join("schema.json"), "{\"type\":\"object\"}").unwrap();

        let a = load(dir.path().to_str().unwrap(), "ns/act").unwrap();
        assert_eq!(a.spec.model, "haiku");
        assert_eq!(a.schema.as_deref(), Some("{\"type\":\"object\"}"));
        assert!(a.spec.connector.is_some());
        // An action that says nothing about posture is a plain completion, as it always was.
        assert_eq!(a.spec.mode, Mode::Generate);
        assert!(a.spec.workspace.is_none());
        assert!(a.spec.allowed_tools.is_empty());
        assert_eq!(a.spec.timeout(), invocation::DEFAULT_TIMEOUT);
        // An action that says nothing about reporting keeps its prompt and result on the device.
        assert!(!a.spec.report_io);
        assert!(a.spec.version.is_none());
        assert!(load(dir.path().to_str().unwrap(), "ns/missing").is_err());
    }

    /// Opting in is explicit, per action, and independent of the version label.
    #[test]
    fn an_action_can_version_itself_and_opt_into_reporting_its_io() {
        let dir = tempfile::tempdir().unwrap();
        let act = dir.path().join("ns").join("judged");
        std::fs::create_dir_all(&act).unwrap();
        std::fs::write(act.join("prompt.md"), "rate this").unwrap();
        std::fs::write(
            act.join("action.toml"),
            "version = \"3\"\nreport_io = true\n",
        )
        .unwrap();

        let a = load(dir.path().to_str().unwrap(), "ns/judged").unwrap();
        assert_eq!(a.spec.version.as_deref(), Some("3"));
        assert!(a.spec.report_io);
    }

    #[test]
    fn an_action_can_declare_its_whole_posture() {
        let dir = tempfile::tempdir().unwrap();
        let act = dir.path().join("ns").join("scan");
        std::fs::create_dir_all(&act).unwrap();
        std::fs::write(act.join("prompt.md"), "look").unwrap();
        std::fs::write(
            act.join("action.toml"),
            "mode = \"readonly-scan\"\nworkspace = \"my-repo\"\nallowed_tools = [\"Bash(git log:*)\"]\n\
             permission_mode = \"plan\"\nmax_budget_usd = 0.5\ntimeout_secs = 120\n",
        )
        .unwrap();

        let a = load(dir.path().to_str().unwrap(), "ns/scan").unwrap();
        assert_eq!(a.spec.mode, Mode::ReadonlyScan);
        assert_eq!(a.spec.workspace.as_deref(), Some("my-repo"));
        assert_eq!(a.spec.allowed_tools, vec!["Bash(git log:*)"]);
        assert_eq!(a.spec.permission_mode.as_deref(), Some("plan"));
        assert_eq!(a.spec.max_budget_usd, Some(0.5));
        assert_eq!(a.spec.timeout(), Duration::from_secs(120));
    }

    #[test]
    fn a_workspace_cannot_escape_its_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("repo")).unwrap();
        let r = Some(root.path());

        assert_eq!(
            resolve_workspace(r, Some("repo"), Mode::Edit).unwrap(),
            Some(root.path().join("repo"))
        );
        for bad in ["../repo", "/abs", "a\\b", "repo/", "", "a/../b"] {
            assert!(
                resolve_workspace(r, Some(bad), Mode::Edit).is_err(),
                "'{bad}' should be rejected"
            );
        }
        // Named but absent, no root configured, missing entirely, or named by a generate action.
        assert!(resolve_workspace(r, Some("nope"), Mode::Edit).is_err());
        assert!(resolve_workspace(None, Some("repo"), Mode::Edit).is_err());
        assert!(resolve_workspace(r, None, Mode::ReadonlyScan).is_err());
        assert!(resolve_workspace(r, Some("repo"), Mode::Generate).is_err());
        assert_eq!(resolve_workspace(r, None, Mode::Generate).unwrap(), None);
    }

    #[test]
    fn expand_env_fills_and_fails_loudly() {
        std::env::set_var("LT_TEST_TOKEN", "s3cret");
        assert_eq!(
            expand_env("Bearer ${LT_TEST_TOKEN}").unwrap(),
            "Bearer s3cret"
        );
        assert!(expand_env("${LT_TEST_MISSING_VAR}").is_err());
        assert_eq!(expand_env("plain").unwrap(), "plain");
    }
}
