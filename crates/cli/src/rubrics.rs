//! Rubrics — the weighted, anchored scoring contract the judge grades against
//! (`docs/BENCHMARK_FRAMEWORK.md` §3).
//!
//! A rubric is structured data (dimensions, weights, gating floors, deterministic checks), so
//! `create` reads it from a JSON file rather than from a wall of flags: the file is the artifact you
//! keep in version control next to the prompts it grades.

use std::fs;

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cli::{Cli, RubricsCmd};
use crate::http::call;

pub(crate) fn run(cli: &Cli, action: &RubricsCmd) -> Result<()> {
    match action {
        RubricsCmd::Create {
            project,
            file,
            name,
            threshold,
        } => {
            let text = fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
            let src: Value =
                serde_json::from_str(&text).with_context(|| format!("{file}: invalid JSON"))?;
            let body = build_body(src, name.as_deref(), *threshold)?;
            call(
                cli,
                Method::POST,
                &format!("/v1/projects/{project}/rubrics"),
                Some(body),
                "get_rubric",
            )
        }
        RubricsCmd::List { project } => call(
            cli,
            Method::GET,
            &format!("/v1/projects/{project}/rubrics"),
            None,
            "list_rubrics",
        ),
        RubricsCmd::Show { id } => call(
            cli,
            Method::GET,
            &format!("/v1/rubrics/{id}"),
            None,
            "get_rubric",
        ),
        RubricsCmd::Version {
            id,
            file,
            threshold,
        } => {
            let body = version_body(file.as_deref(), *threshold)?;
            call(
                cli,
                Method::POST,
                &format!("/v1/rubrics/{id}/versions"),
                Some(body),
                "get_rubric",
            )
        }
    }
}

/// Build the new-version body. Both halves are optional and an omitted one is **absent**, not a
/// guessed default: the server carries the superseded rubric's value forward, which is the only
/// place that knows it. A version that changes nothing at all is refused here rather than minting a
/// duplicate row nobody asked for.
fn version_body(file: Option<&str>, threshold: Option<f64>) -> Result<Value> {
    let mut body = json!({});
    if let Some(f) = file {
        let text = fs::read_to_string(f).with_context(|| format!("reading {f}"))?;
        let src: Value =
            serde_json::from_str(&text).with_context(|| format!("{f}: invalid JSON"))?;
        let dims = match src {
            Value::Array(dims) => dims,
            Value::Object(mut o) => match o.remove("dimensions") {
                Some(Value::Array(dims)) => dims,
                _ => bail!("{f}: object form needs a \"dimensions\" array"),
            },
            _ => bail!("{f}: expected a dimensions array or an object carrying one"),
        };
        if dims.is_empty() {
            bail!("{f}: a rubric version needs at least one dimension");
        }
        body["dimensions"] = Value::Array(dims);
    }
    if let Some(t) = threshold {
        body["threshold"] = json!(t);
    }
    if body.as_object().is_none_or(|o| o.is_empty()) {
        bail!("nothing to change: pass --file, --threshold, or both");
    }
    Ok(body)
}

/// Build the create-rubric body from the file plus flag overrides.
///
/// The file may be the whole request (`{name, threshold, dimensions}`) or just the bare
/// `[dimensions]` array — the array form is what you get when a rubric is templated or generated,
/// and `--name` then supplies the missing field. Both required fields are checked here so a
/// malformed rubric fails at the keyboard with a message naming the fix, instead of as a 422 from
/// serde on the far side of the wire.
fn build_body(src: Value, name: Option<&str>, threshold: Option<f64>) -> Result<Value> {
    let mut body = match src {
        Value::Array(dims) => json!({ "dimensions": dims }),
        Value::Object(_) => src,
        _ => bail!(
            "rubric file must be a JSON object {{\"name\", \"threshold\", \"dimensions\"}} or a \
             bare array of dimensions"
        ),
    };
    if let Some(n) = name {
        body["name"] = json!(n);
    }
    if let Some(t) = threshold {
        body["threshold"] = json!(t);
    }
    if body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        bail!("rubric needs a name: put \"name\" in the file, or pass --name");
    }
    match body.get("dimensions").and_then(Value::as_array) {
        Some(d) if !d.is_empty() => {}
        _ => bail!(
            "rubric needs a non-empty \"dimensions\" array — each {{key, description, weight, \
             floor?, anchors?, kind?, check?}} (see docs/BENCHMARK_FRAMEWORK.md §3)"
        ),
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A version must carry only what actually changed: an omitted half is absent so the server
    /// carries it forward, and a version that changes nothing is refused before the request.
    #[test]
    fn a_version_body_carries_only_what_changed() {
        let body = version_body(None, Some(0.85)).expect("threshold-only version");
        assert_eq!(body["threshold"], 0.85);
        assert!(
            body.get("dimensions").is_none(),
            "dimensions are carried forward by the server, not restated here: {body}"
        );
        assert!(
            version_body(None, None).is_err(),
            "a no-op version would mint a duplicate row nobody asked for"
        );
    }

    fn dims() -> Value {
        json!([{ "key": "correctness", "description": "right?", "weight": 3.0, "floor": 0.5 }])
    }

    #[test]
    fn object_form_passes_through_with_overrides_applied() {
        let src = json!({ "name": "from-file", "threshold": 0.7, "dimensions": dims() });
        let body = build_body(src, None, None).expect("object form");
        assert_eq!(body["name"], "from-file");
        assert_eq!(body["threshold"], 0.7);
        // Flags win over the file, so one checked-in rubric can be created at several thresholds.
        let src = json!({ "name": "from-file", "threshold": 0.7, "dimensions": dims() });
        let body = build_body(src, Some("override"), Some(0.9)).expect("overrides");
        assert_eq!(body["name"], "override");
        assert_eq!(body["threshold"], 0.9);
    }

    /// The bare-array form is the generated/templated shape; `--name` is what makes it complete.
    #[test]
    fn bare_dimension_array_is_wrapped_and_needs_a_name() {
        let body = build_body(dims(), Some("support-quality"), None).expect("array form");
        assert_eq!(body["name"], "support-quality");
        assert_eq!(body["dimensions"].as_array().expect("dims").len(), 1);
        // No threshold set anywhere → the field is absent and the API applies its own default (0.7),
        // rather than the CLI inventing a second default that could drift from it.
        assert!(body.get("threshold").is_none());
        assert!(build_body(dims(), None, None).is_err(), "no name anywhere");
    }

    #[test]
    fn a_rubric_with_no_dimensions_is_refused_before_the_request() {
        let err = build_body(json!({ "name": "empty", "dimensions": [] }), None, None)
            .expect_err("empty dimensions");
        assert!(err.to_string().contains("dimensions"), "got: {err}");
        let err = build_body(json!({ "name": "none" }), None, None).expect_err("no dimensions");
        assert!(err.to_string().contains("dimensions"), "got: {err}");
        let err = build_body(json!("a string"), Some("x"), None).expect_err("wrong JSON shape");
        assert!(
            err.to_string().contains("must be a JSON object"),
            "got: {err}"
        );
    }
}
