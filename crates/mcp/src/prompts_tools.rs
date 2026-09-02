//! Score-recording and prompt-registry tools.
//!
//! Reads (`list_prompts`, `get_prompt`) are always available; writes (`record_score`, `score_trace`,
//! `create_prompt_version`, `promote_prompt`) are gated behind `LIGHTTRACK_MCP_ALLOW_WRITES` like every
//! other write. This is where "the agent manages prompts, gated by benchmarks" becomes usable: a new
//! version auto-enqueues the linked benchmark, and `promote_prompt` is refused (409) when that
//! benchmark regressed — surfaced to the agent as a clear verdict by [`crate::errors::map_error`].
//!
//! The server stays a thin HTTP client: each tool just assembles a typed body and forwards it to the
//! validating API. No key-minting here.

use serde_json::{json, Map, Value};

use crate::client::Client;

/// True if `name` is one of this module's write tools.
/// Route a read tool. `None` if `name` is not one of ours.
pub(crate) fn read_dispatch(c: &Client, name: &str, args: &Value) -> Option<Result<Value, String>> {
    let r = match name {
        "list_prompts" => match need(args, "project") {
            Ok(p) => c.get(&format!("/v1/projects/{p}/prompts")),
            Err(e) => Err(e),
        },
        "get_prompt" => match (need(args, "project"), need(args, "name")) {
            (Ok(p), Ok(n)) => c.get(&get_prompt_path(&p, &n, args)),
            (Err(e), _) | (_, Err(e)) => Err(e),
        },
        "get_prompt_quality" => match need(args, "project") {
            Ok(p) => c.get(&quality_path(&p, args)),
            Err(e) => Err(e),
        },
        _ => return None,
    };
    Some(r)
}

/// Route a write tool. `None` if `name` is not one of ours.
pub(crate) fn write_dispatch(
    c: &Client,
    name: &str,
    args: &Value,
) -> Option<Result<Value, String>> {
    let r = match name {
        "record_score" => match score_body(args, None) {
            Ok(body) => c.post("/v1/scores", &body),
            Err(e) => Err(e),
        },
        "score_trace" => match need(args, "trace") {
            Ok(t) => match score_body(args, Some(&t)) {
                Ok(body) => c.post(&format!("/v1/traces/{t}/score"), &body),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        },
        "create_prompt_version" => create_prompt_version(c, args),
        "promote_prompt" => promote_prompt(c, args),
        _ => return None,
    };
    Some(r)
}

/// Build the `GET /v1/quality/prompts` query. Every narrowing is optional: "how are my served
/// versions doing" has a useful answer before an agent knows which window or rubric to ask about.
fn quality_path(project: &str, args: &Value) -> String {
    let mut p = format!("/v1/quality/prompts?project={project}");
    for k in ["since", "until", "rubric_id"] {
        if let Some(v) = args
            .get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            p.push_str(&format!("&{k}={v}"));
        }
    }
    p
}

/// Build the `GET /v1/projects/:id/prompts/:name` path with an optional `label`/`version` selector.
fn get_prompt_path(project: &str, name: &str, args: &Value) -> String {
    let mut p = format!("/v1/projects/{project}/prompts/{name}");
    if let Some(v) = args.get("version").and_then(Value::as_u64) {
        p.push_str(&format!("?version={v}"));
    } else if let Some(l) = args
        .get("label")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        p.push_str(&format!("?label={l}"));
    }
    p
}

/// Assemble a `Score` / trace-score body from `args`. Requires `rubric` + `value`; defaults `scored_by`
/// to `mcp` (the API rejects a score without one). When `trace` is `None` this is a bare `/v1/scores`
/// post, so a `project` arg maps to the `project_id` field the API resolves.
fn score_body(args: &Value, trace: Option<&str>) -> Result<Value, String> {
    if let Some(e) = missing(args, &["rubric", "value"]) {
        return Err(e);
    }
    let mut m = Map::new();
    copy(
        &mut m,
        args,
        &["rubric", "value", "max", "pass", "reasoning", "cost_usd"],
    );
    // `event` is the agent-facing name; the API field is `event_id`.
    if let Some(ev) = args
        .get("event")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        m.insert("event_id".into(), json!(ev));
    }
    let scored_by = args
        .get("scored_by")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    m.insert("scored_by".into(), json!(scored_by.unwrap_or("mcp")));
    // A trace supplies its own project; a bare score may need an explicit one (admin key).
    if trace.is_none() {
        if let Some(p) = args
            .get("project")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            m.insert("project_id".into(), json!(p));
        }
    }
    Ok(Value::Object(m))
}

/// Add a version to a registry prompt, creating the prompt on first use. Posts to the `/versions`
/// endpoint; if that 404s (the prompt does not exist yet) it falls back to creating the prompt with
/// the same content — so a single tool both creates and versions.
fn create_prompt_version(c: &Client, args: &Value) -> Result<Value, String> {
    let project = need(args, "project")?;
    let name = need(args, "name")?;
    if let Some(e) = missing(args, &["content"]) {
        return Err(e);
    }
    let mut version_body = Map::new();
    copy(&mut version_body, args, &["content", "config", "note"]);
    let versions_path = format!("/v1/projects/{project}/prompts/{name}/versions");
    match c.post(&versions_path, &Value::Object(version_body.clone())) {
        Err(e) if is_status(&e, 404) => {
            // Prompt doesn't exist yet — create it (version 1) with the same content.
            let mut create_body = version_body;
            create_body.insert("name".into(), json!(name));
            copy(&mut create_body, args, &["benchmark_id"]);
            c.post(
                &format!("/v1/projects/{project}/prompts"),
                &Value::Object(create_body),
            )
        }
        other => other,
    }
}

/// Point a label at a version. Requires project/name/label/version; the API's 409 regression verdict
/// is surfaced verbatim (plus a plain-language line) by the error mapper.
fn promote_prompt(c: &Client, args: &Value) -> Result<Value, String> {
    let project = need(args, "project")?;
    let name = need(args, "name")?;
    if let Some(e) = missing(args, &["label", "version"]) {
        return Err(e);
    }
    let mut body = Map::new();
    copy(&mut body, args, &["label", "version", "force"]);
    c.post(
        &format!("/v1/projects/{project}/prompts/{name}/promote"),
        &Value::Object(body),
    )
}

/// Require a non-empty string arg.
fn need(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("missing required argument: {key}"))
}

/// First missing/null required key, or `None` if all present.
fn missing(args: &Value, required: &[&str]) -> Option<String> {
    required
        .iter()
        .find(|k| args.get(**k).is_none_or(Value::is_null))
        .map(|k| format!("missing required argument: {k}"))
}

/// Copy each present, non-null `key` from `args` into `m`.
fn copy(m: &mut Map<String, Value>, args: &Value, keys: &[&str]) {
    for k in keys {
        if let Some(v) = args.get(*k) {
            if !v.is_null() {
                m.insert((*k).to_string(), v.clone());
            }
        }
    }
}

/// True when a client error string carries the given HTTP status (`HTTP {code}: …`).
fn is_status(err: &str, code: u16) -> bool {
    err.starts_with(&format!("HTTP {code}:"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn score_body_requires_rubric_and_value() {
        assert!(score_body(&json!({}), None).unwrap_err().contains("rubric"));
        assert!(score_body(&json!({ "rubric": "helpfulness" }), None)
            .unwrap_err()
            .contains("value"));
    }

    #[test]
    fn score_body_defaults_scored_by_and_maps_event() {
        let b = score_body(
            &json!({ "rubric": "r", "value": 0.8, "event": "ev-1", "project": "p1" }),
            None,
        )
        .unwrap();
        assert_eq!(b["scored_by"], "mcp");
        assert_eq!(b["event_id"], "ev-1"); // renamed from `event`
        assert_eq!(b["project_id"], "p1"); // renamed from `project`
        assert!(b.get("event").is_none());
    }

    #[test]
    fn score_body_for_trace_omits_project() {
        // A trace supplies its own project — don't smuggle a project_id in.
        let b = score_body(
            &json!({ "rubric": "r", "value": 1.0, "project": "p1", "scored_by": "haiku" }),
            Some("tr-1"),
        )
        .unwrap();
        assert!(b.get("project_id").is_none());
        assert_eq!(b["scored_by"], "haiku");
    }

    #[test]
    fn get_prompt_path_prefers_version_over_label() {
        assert_eq!(
            get_prompt_path("p", "greeting", &json!({})),
            "/v1/projects/p/prompts/greeting"
        );
        assert_eq!(
            get_prompt_path("p", "greeting", &json!({ "label": "production" })),
            "/v1/projects/p/prompts/greeting?label=production"
        );
        assert_eq!(
            get_prompt_path(
                "p",
                "greeting",
                &json!({ "label": "production", "version": 2 })
            ),
            "/v1/projects/p/prompts/greeting?version=2"
        );
    }

    #[test]
    fn dispatch_validates_required_before_http() {
        // `need`/`missing` fail first, so no request touches the client's base URL.
        let c = Client::from_env();
        assert!(read_dispatch(&c, "list_prompts", &json!({}))
            .unwrap()
            .unwrap_err()
            .contains("project"));
        assert!(write_dispatch(&c, "record_score", &json!({}))
            .unwrap()
            .unwrap_err()
            .contains("rubric"));
        assert!(write_dispatch(
            &c,
            "promote_prompt",
            &json!({ "project": "p", "name": "n" })
        )
        .unwrap()
        .unwrap_err()
        .contains("label"));
        assert!(write_dispatch(
            &c,
            "create_prompt_version",
            &json!({ "project": "p", "name": "n" })
        )
        .unwrap()
        .unwrap_err()
        .contains("content"));
    }

    #[test]
    fn dispatch_returns_none_for_foreign_tools() {
        let c = Client::from_env();
        assert!(read_dispatch(&c, "list_projects", &json!({})).is_none());
        assert!(write_dispatch(&c, "create_project", &json!({})).is_none());
    }

    #[test]
    fn is_status_matches_prefix_only() {
        assert!(is_status("HTTP 404: not found", 404));
        assert!(!is_status("HTTP 409: conflict", 404));
        assert!(!is_status("transport error", 404));
    }
}
