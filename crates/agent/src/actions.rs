//! The local action library: `<actions_dir>/<action_type>/` holds `prompt.md` (a template with
//! `{{…}}` placeholders), `action.toml` (model + options + connector), and optionally
//! `schema.json`. The library is the device-side half of the relay contract — the cloud only ever
//! names an `action_type` and supplies params; everything executable lives here, gitignored.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use lighttrack_core::RelayTask;

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
    #[serde(default)]
    pub connector: Option<ConnectorSpec>,
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

pub(crate) fn load(actions_dir: &str, action_type: &str) -> Result<Action> {
    validate_action_type(action_type)?;
    let dir: PathBuf = Path::new(actions_dir).join(action_type);
    if !dir.is_dir() {
        bail!("no action '{action_type}' in library '{actions_dir}' (expected {})", dir.display());
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
    Ok(Action { spec, prompt_template, schema })
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
pub(crate) fn expand_headers(headers: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
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
        assert!(load(dir.path().to_str().unwrap(), "ns/missing").is_err());
    }

    #[test]
    fn expand_env_fills_and_fails_loudly() {
        std::env::set_var("LT_TEST_TOKEN", "s3cret");
        assert_eq!(expand_env("Bearer ${LT_TEST_TOKEN}").unwrap(), "Bearer s3cret");
        assert!(expand_env("${LT_TEST_MISSING_VAR}").is_err());
        assert_eq!(expand_env("plain").unwrap(), "plain");
    }
}
