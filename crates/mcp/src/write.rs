//! Write tools — state-changing POST/PUT operations (enqueue benchmark runs, create
//! projects/datasets/rubrics/benchmarks, set limits/prices).
//!
//! **Dispatch only**: the catalog (names, descriptions, input schemas, annotations) is generated
//! from `lighttrack-contract`, and which tools are writes is derived from each endpoint's
//! `mutating` flag rather than from a second hand-kept name list — that list is what drifted.
//!
//! These are gated behind `LIGHTTRACK_MCP_ALLOW_WRITES` (see `tools::call`) and annotated
//! `readOnlyHint: false` so a client/agent treats them with care. The bodies are forwarded to the
//! API, which validates them — the MCP server cannot bypass that. Note: minting API keys is
//! deliberately *not* exposed here, to avoid leaking secrets into an agent's context. Alert
//! CHANNELS are excluded for exactly the same reason — creating one returns a webhook signing
//! secret exactly once, and that would land verbatim in a transcript — and so is
//! `POST /v1/alerts/:id/resolution`, which is the responder's door, not an agent's.

use serde_json::{json, Value};

use crate::client::Client;

/// Route a write tool. Returns `None` if `name` is not a write tool.
pub(crate) fn dispatch(c: &Client, name: &str, args: &Value) -> Option<Result<Value, String>> {
    let r = match name {
        "enqueue_benchmark" => match need(args, "benchmark") {
            Ok(b) => c.post(
                &format!("/v1/benchmarks/{b}/enqueue"),
                &pick(args, &["samples", "heal"]),
            ),
            Err(e) => Err(e),
        },
        "enqueue_job" => match need(args, "type") {
            Ok(_) => c.post("/v1/jobs", &pick(args, &["type", "payload"])),
            Err(e) => Err(e),
        },
        "create_schedule" => match need(args, "project") {
            Ok(p) => post_with(
                c,
                args,
                &["type", "interval_secs"],
                &[
                    "type",
                    "payload",
                    "interval_secs",
                    "start_in_secs",
                    "enabled",
                ],
                format!("/v1/projects/{p}/schedules"),
            ),
            Err(e) => Err(e),
        },
        "create_project" => post_with(
            c,
            args,
            &["name"],
            &["name", "redaction"],
            "/v1/projects".to_string(),
        ),
        "create_dataset" => match need(args, "project") {
            Ok(p) => post_with(
                c,
                args,
                &["name"],
                &["name", "source"],
                format!("/v1/projects/{p}/datasets"),
            ),
            Err(e) => Err(e),
        },
        "add_dataset_item" => match need(args, "dataset") {
            Ok(d) => post_with(
                c,
                args,
                &["input"],
                &["input", "output", "expected", "context", "tags"],
                format!("/v1/datasets/{d}/items"),
            ),
            Err(e) => Err(e),
        },
        "freeze_dataset" => match need(args, "dataset") {
            Ok(d) => c.post(&format!("/v1/datasets/{d}/freeze"), &json!({})),
            Err(e) => Err(e),
        },
        "fork_dataset" => match need(args, "dataset") {
            Ok(d) => c.post(&format!("/v1/datasets/{d}/fork"), &json!({})),
            Err(e) => Err(e),
        },
        "import_dataset_items" => match need(args, "dataset") {
            Ok(d) => c.post(
                &format!("/v1/datasets/{d}/items/import"),
                &import_spec(args),
            ),
            Err(e) => Err(e),
        },
        "record_label" => post_with(
            c,
            args,
            &["subject", "value", "labeler"],
            &[
                "project_id",
                "subject",
                "value",
                "pass",
                "rubric_id",
                "labeler",
                "note",
            ],
            "/v1/labels".to_string(),
        ),
        "create_rubric" => match need(args, "project") {
            Ok(p) => post_with(
                c,
                args,
                &["name", "dimensions"],
                &["name", "dimensions", "threshold"],
                format!("/v1/projects/{p}/rubrics"),
            ),
            Err(e) => Err(e),
        },
        "create_benchmark" => match need(args, "project") {
            Ok(p) => post_with(
                c,
                args,
                &["name"],
                &[
                    "name",
                    "rubric",
                    "rubric_id",
                    "judge_model",
                    "dataset_ref",
                    "dataset",
                    "targets",
                    "baseline_score",
                ],
                format!("/v1/projects/{p}/benchmarks"),
            ),
            Err(e) => Err(e),
        },
        "ack_alert" => match need(args, "id") {
            Ok(id) => c.post(&format!("/v1/alerts/{id}/ack"), &pick(args, &["by"])),
            Err(e) => Err(e),
        },
        "create_limit" => match need(args, "project") {
            Ok(p) => post_with(
                c,
                args,
                &["metric", "window", "threshold"],
                &["metric", "window", "threshold", "action"],
                format!("/v1/projects/{p}/limits"),
            ),
            Err(e) => Err(e),
        },
        "update_limit" => match need(args, "id") {
            Ok(id) => post_put(
                c,
                args,
                &["metric", "window", "threshold"],
                &[
                    "metric",
                    "window",
                    "threshold",
                    "action",
                    "enabled",
                    "warn_at",
                    "scope",
                ],
                format!("/v1/limits/{id}"),
            ),
            Err(e) => Err(e),
        },
        "delete_limit" => match need(args, "id") {
            Ok(id) => c.delete(&format!("/v1/limits/{id}")),
            Err(e) => Err(e),
        },
        "put_price" => match (need(args, "provider"), need(args, "model")) {
            (Ok(p), Ok(m)) => {
                let required = &["input_per_mtok", "output_per_mtok"];
                match missing(args, required) {
                    Some(e) => Err(e),
                    None => c.put(
                        &format!("/v1/prices/{p}/{m}"),
                        &pick(
                            args,
                            &[
                                "input_per_mtok",
                                "output_per_mtok",
                                "cached_input_per_mtok",
                                "source_url",
                            ],
                        ),
                    ),
                }
            }
            (Err(e), _) | (_, Err(e)) => Err(e),
        },
        _ => return None,
    };
    Some(r)
}

/// POST `body_keys` from `args` to `path`, after asserting `required` are present.
fn post_with(
    c: &Client,
    args: &Value,
    required: &[&str],
    body_keys: &[&str],
    path: String,
) -> Result<Value, String> {
    match missing(args, required) {
        Some(e) => Err(e),
        None => c.post(&path, &pick(args, body_keys)),
    }
}

/// PUT `body_keys` from `args` to `path`, after asserting `required` are present (replace semantics).
fn post_put(
    c: &Client,
    args: &Value,
    required: &[&str],
    body_keys: &[&str],
    path: String,
) -> Result<Value, String> {
    match missing(args, required) {
        Some(e) => Err(e),
        None => c.put(&path, &pick(args, body_keys)),
    }
}

/// Require a string arg.
/// Build the `ImportSpec` body from `import_dataset_items`' flat arguments.
///
/// Flat on the wire and nested on the way out, deliberately: the API's spec nests the row predicates
/// under `filter`, and asking an agent to construct a two-level object correctly — for a tool it
/// will call once — is how a tool call becomes three attempts. The nesting is mechanical, so it
/// happens here.
fn import_spec(args: &Value) -> Value {
    let mut filter = serde_json::Map::new();
    for k in ["model", "status", "since", "below", "pass"] {
        if let Some(v) = args.get(k) {
            if !v.is_null() {
                filter.insert(k.to_string(), v.clone());
            }
        }
    }
    let mut spec = pick(args, &["from", "strategy", "n", "dedupe", "event_ids"]);
    if let Some(m) = spec.as_object_mut() {
        if !filter.is_empty() {
            m.insert("filter".to_string(), Value::Object(filter));
        }
    }
    spec
}

fn need(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing required argument: {key}"))
}

/// First missing/null required key, as an error, or `None` if all present.
fn missing(args: &Value, required: &[&str]) -> Option<String> {
    required
        .iter()
        .find(|k| args.get(**k).is_none_or(Value::is_null))
        .map(|k| format!("missing required argument: {k}"))
}

/// Build a JSON object from `args`, copying each present (non-null) key in `keys`.
fn pick(args: &Value, keys: &[&str]) -> Value {
    let mut m = serde_json::Map::new();
    for k in keys {
        if let Some(v) = args.get(*k) {
            if !v.is_null() {
                m.insert((*k).to_string(), v.clone());
            }
        }
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dispatch_requires_id_before_any_http() {
        // `need(id)` fails first, so no request is attempted against the client's base URL.
        let c = Client::from_env();
        for tool in ["update_limit", "delete_limit"] {
            let r = dispatch(&c, tool, &json!({})).expect("is a write tool");
            assert!(r.unwrap_err().contains("id"), "{tool} should require id");
        }
    }

    #[test]
    fn pick_copies_present_non_null_keys() {
        let body = pick(
            &json!({ "metric": "cost_usd", "threshold": 5.0, "action": null }),
            &["metric", "threshold", "action", "scope"],
        );
        assert_eq!(body["metric"], "cost_usd");
        assert_eq!(body["threshold"], 5.0);
        assert!(body.get("action").is_none()); // null skipped
        assert!(body.get("scope").is_none()); // absent skipped
    }
}
