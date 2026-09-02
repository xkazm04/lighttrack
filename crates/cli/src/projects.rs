//! Projects and their API keys (admin-only on the API side).
//!
//! `keys create` and `keys rotate` print a secret exactly once — only a salted digest is stored, so
//! a lost key is replaced, never recovered. That is why key minting lives here and not over MCP: a
//! key in a tool result is a key in an agent transcript.

use anyhow::{bail, Result};
use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::cli::{Cli, KeysCmd, ProjectsCmd};
use crate::http::call;
use crate::query::Query;

pub(crate) fn run(cli: &Cli, action: &ProjectsCmd) -> Result<()> {
    match action {
        // `id` is sent only when the operator chose one: an explicit `null` would be a supplied id
        // as far as a stricter server is concerned, and the server's UUID is the right default.
        ProjectsCmd::Create {
            name,
            id,
            redaction,
        } => {
            let mut body = json!({ "name": name });
            if let Some(id) = id {
                body["id"] = json!(id);
            }
            if let Some(r) = redaction {
                body["redaction"] = json!(r);
            }
            call(cli, Method::POST, "/v1/projects", Some(body), "")
        }
        ProjectsCmd::List => call(cli, Method::GET, "/v1/projects", None, "list_projects"),
        ProjectsCmd::Update {
            id,
            name,
            enable,
            disable,
            redaction,
            collective_opt_in,
            require_trusted_judge,
        } => {
            let body = update_body(
                name.as_deref(),
                *enable,
                *disable,
                redaction.as_deref(),
                *collective_opt_in,
                *require_trusted_judge,
            )?;
            call(
                cli,
                Method::PUT,
                &format!("/v1/projects/{id}"),
                Some(body),
                "",
            )
        }
        ProjectsCmd::Archive { id } => {
            call(cli, Method::DELETE, &format!("/v1/projects/{id}"), None, "")
        }
        ProjectsCmd::Redaction { id, since } => {
            let mut q = Query::new(&format!("/v1/projects/{id}/redaction"));
            q.push("since", since.as_deref());
            call(cli, Method::GET, &q.finish(), None, "")
        }
    }
}

/// A field the operator did not name is absent, not null: the route leaves an omitted field as it
/// was, and sending `null` would be an instruction to clear it.
fn update_body(
    name: Option<&str>,
    enable: bool,
    disable: bool,
    redaction: Option<&str>,
    collective_opt_in: Option<bool>,
    require_trusted_judge: Option<bool>,
) -> Result<Value> {
    let mut body = Map::new();
    if let Some(n) = name {
        body.insert("name".into(), json!(n));
    }
    if enable || disable {
        body.insert("enabled".into(), json!(enable));
    }
    if let Some(r) = redaction {
        body.insert("redaction".into(), json!(r));
    }
    if let Some(v) = collective_opt_in {
        body.insert("collective_opt_in".into(), json!(v));
    }
    if let Some(v) = require_trusted_judge {
        body.insert("require_trusted_judge".into(), json!(v));
    }
    if body.is_empty() {
        bail!("nothing to change: pass --name, --enable/--disable, --redaction, --collective-opt-in or --require-trusted-judge");
    }
    Ok(Value::Object(body))
}

pub(crate) fn run_keys(cli: &Cli, action: &KeysCmd) -> Result<()> {
    match action {
        // `scopes` and `expires_at` are sent only when the operator named them: an explicit `null`
        // would read as "no scopes" to a stricter server, and the omitted-field default is the
        // documented back-compat one.
        KeysCmd::Create {
            project,
            name,
            scopes,
            expires,
        } => {
            let mut body = json!({ "name": name });
            if !scopes.is_empty() {
                body["scopes"] = json!(scopes);
            }
            if let Some(e) = expires {
                body["expires_at"] = json!(e);
            }
            call(
                cli,
                Method::POST,
                &format!("/v1/projects/{project}/keys"),
                Some(body),
                "",
            )
        }
        KeysCmd::List { project } => call(
            cli,
            Method::GET,
            &format!("/v1/projects/{project}/keys"),
            None,
            "",
        ),
        KeysCmd::Rotate {
            project,
            id,
            grace_secs,
        } => {
            let mut body = json!({});
            if let Some(g) = grace_secs {
                body["grace_secs"] = json!(g);
            }
            call(
                cli,
                Method::POST,
                &format!("/v1/projects/{project}/keys/{id}/rotate"),
                Some(body),
                "",
            )
        }
        KeysCmd::Revoke { project, id } => call(
            cli,
            Method::DELETE,
            &format!("/v1/projects/{project}/keys/{id}"),
            None,
            "",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An update that names nothing is a PUT that would look like a no-op but is really an operator
    /// mistake; refuse it here rather than round-tripping.
    #[test]
    fn an_empty_update_is_refused() {
        assert!(update_body(None, false, false, None, None, None).is_err());
    }

    /// `--disable` has to reach the wire as `enabled: false`; without either flag the field must be
    /// absent, so a rename cannot silently re-enable an archived project.
    #[test]
    fn enablement_is_sent_only_when_a_flag_asked_for_it() {
        let b = update_body(Some("new"), false, false, None, None, None).expect("body");
        assert_eq!(b["name"], json!("new"));
        assert!(b.get("enabled").is_none(), "{b}");

        let b = update_body(None, false, true, None, None, None).expect("body");
        assert_eq!(b["enabled"], json!(false));
        let b = update_body(None, true, false, None, None, None).expect("body");
        assert_eq!(b["enabled"], json!(true));
    }

    /// An explicit `false` on a policy flag is a real instruction and must survive; it is not the
    /// same as leaving the flag off.
    #[test]
    fn an_explicit_false_policy_flag_is_sent() {
        let b = update_body(None, false, false, None, Some(false), None).expect("body");
        assert_eq!(b["collective_opt_in"], json!(false));
        assert!(b.get("require_trusted_judge").is_none(), "{b}");
    }
}
