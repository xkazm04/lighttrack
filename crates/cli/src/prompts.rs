//! `lt prompts` — the registry's read side, the write side that cuts a version, and the one
//! question promotion used to leave unanswered: is the version we promoted actually any good?

use std::fs;

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::cli::{Cli, PromptsCmd};
use crate::http::call;
use crate::query::encode;

/// `/v1/quality/prompts` with the optional narrowings. Every parameter is optional: "how are my
/// served versions scoring" has a useful answer before an operator knows which window or rubric to
/// ask about.
pub(crate) fn quality_path(
    project: &Option<String>,
    since: &Option<String>,
    until: &Option<String>,
    rubric_id: &Option<String>,
) -> String {
    let mut p = "/v1/quality/prompts".to_string();
    let mut sep = '?';
    for (k, v) in [
        ("project", project),
        ("since", since),
        ("until", until),
        ("rubric_id", rubric_id),
    ] {
        if let Some(val) = v.as_deref().filter(|s| !s.is_empty()) {
            p.push_str(&format!("{sep}{k}={val}"));
            sep = '&';
        }
    }
    p
}

pub(crate) fn run(cli: &Cli, action: &PromptsCmd) -> Result<()> {
    match action {
        PromptsCmd::List { project } => call(
            cli,
            Method::GET,
            &format!("/v1/projects/{project}/prompts"),
            None,
            "list_prompts",
        ),
        PromptsCmd::Quality {
            project,
            since,
            until,
            rubric_id,
        } => call(
            cli,
            Method::GET,
            &quality_path(project, since, until, rubric_id),
            None,
            "get_prompt_quality",
        ),
        PromptsCmd::Create {
            project,
            name,
            file,
            config,
            note,
            benchmark_id,
        } => {
            let content = fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
            let body = create_body(
                name,
                &content,
                config.as_deref(),
                note.as_deref(),
                benchmark_id.as_deref(),
            )?;
            call(
                cli,
                Method::POST,
                &format!("/v1/projects/{project}/prompts"),
                Some(body),
                "get_prompt",
            )
        }
        // An absent `--benchmark-id` is an explicit `null`: this route's only field IS the link, so
        // omitting it would be a PUT that means nothing, while unlinking has to be sayable.
        PromptsCmd::Link {
            project,
            name,
            benchmark_id,
        } => call(
            cli,
            Method::PUT,
            &format!("/v1/projects/{project}/prompts/{}", encode(name)),
            Some(json!({ "benchmark_id": benchmark_id })),
            "get_prompt",
        ),
        PromptsCmd::Versions { project, name } => call(
            cli,
            Method::GET,
            &format!("/v1/projects/{project}/prompts/{}/versions", encode(name)),
            None,
            "",
        ),
        PromptsCmd::Canary {
            project,
            name,
            file,
            clear,
        } => {
            let body = canary_body(file.as_deref(), *clear)?;
            call(
                cli,
                Method::PUT,
                &format!("/v1/projects/{project}/prompts/{}/canary", encode(name)),
                Some(body),
                "get_prompt",
            )
        }
    }
}

/// The version-1 body. `config` is typed JSON rather than a string, so a malformed one is refused
/// here — stored as text it would deserialize into nothing the runtime can read.
fn create_body(
    name: &str,
    content: &str,
    config: Option<&str>,
    note: Option<&str>,
    benchmark_id: Option<&str>,
) -> Result<Value> {
    let mut body = Map::new();
    body.insert("name".into(), json!(name));
    body.insert("content".into(), json!(content));
    if let Some(c) = config {
        let v: Value =
            serde_json::from_str(c).with_context(|| format!("--config: invalid JSON: {c}"))?;
        if !v.is_object() {
            bail!("--config must be a JSON object, got: {c}");
        }
        body.insert("config".into(), v);
    }
    if let Some(n) = note {
        body.insert("note".into(), json!(n));
    }
    if let Some(b) = benchmark_id {
        body.insert("benchmark_id".into(), json!(b));
    }
    Ok(Value::Object(body))
}

/// `null` is how the route spells "clear the policy", so `--clear` sends it deliberately rather
/// than by omission — and neither flag at all is an operator mistake, not an empty PUT.
fn canary_body(file: Option<&str>, clear: bool) -> Result<Value> {
    if clear {
        return Ok(json!({ "canary": Value::Null }));
    }
    let Some(f) = file else {
        bail!("pass --file <policy.json> to set a canary policy, or --clear to remove it");
    };
    let text = fs::read_to_string(f).with_context(|| format!("reading {f}"))?;
    let policy: Value =
        serde_json::from_str(&text).with_context(|| format!("{f}: invalid JSON"))?;
    if !policy.is_object() {
        bail!("{f}: a canary policy is a JSON object");
    }
    Ok(json!({ "canary": policy }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn the_quality_path_narrows_only_when_asked() {
        assert_eq!(
            quality_path(&None, &None, &None, &None),
            "/v1/quality/prompts"
        );
        assert_eq!(
            quality_path(&s("p1"), &None, &None, &None),
            "/v1/quality/prompts?project=p1"
        );
        // A later parameter alone must open the query string, not join an absent one with `&`.
        assert_eq!(
            quality_path(&None, &None, &None, &s("rub-1")),
            "/v1/quality/prompts?rubric_id=rub-1"
        );
        assert_eq!(
            quality_path(&s("p1"), &s("2026-01-01T00:00:00Z"), &s("2026-02-01T00:00:00Z"), &s("r")),
            "/v1/quality/prompts?project=p1&since=2026-01-01T00:00:00Z&until=2026-02-01T00:00:00Z&rubric_id=r"
        );
        // An empty string is the shape a shell hands over for an unset variable; it must not
        // become `?project=`, which the API would read as a project named "".
        assert_eq!(
            quality_path(&s(""), &None, &None, &None),
            "/v1/quality/prompts"
        );
    }

    /// A registry name is operator data and can carry `/`; unencoded it would move the route.
    #[test]
    fn a_prompt_name_is_encoded_into_the_path() {
        assert_eq!(encode("billing/summary"), "billing%2Fsummary");
    }

    #[test]
    fn create_omits_what_was_not_given_and_refuses_a_bad_config() {
        let b = create_body("greeting", "hello", None, None, None).expect("body");
        assert_eq!(b["content"], json!("hello"));
        assert!(b.get("config").is_none(), "{b}");
        assert!(b.get("benchmark_id").is_none(), "{b}");

        assert!(create_body("g", "hello", Some("{bad"), None, None).is_err());
        assert!(create_body("g", "hello", Some("[1]"), None, None).is_err());
    }

    /// The two halves of the canary verb: `--clear` is an explicit null, and naming neither is an
    /// error rather than a PUT that would clear the policy by accident.
    #[test]
    fn clearing_a_canary_is_explicit_and_naming_nothing_is_refused() {
        assert_eq!(
            canary_body(None, true).expect("body"),
            json!({ "canary": Value::Null })
        );
        assert!(canary_body(None, false).is_err());
    }
}
