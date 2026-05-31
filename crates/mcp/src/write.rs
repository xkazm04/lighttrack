//! Write tools — state-changing POST/PUT operations (enqueue benchmark runs, create
//! projects/datasets/rubrics/benchmarks, set limits/prices).
//!
//! These are gated behind `LIGHTTRACK_MCP_ALLOW_WRITES` (see `tools::call`) and annotated
//! `readOnlyHint: false` so a client/agent treats them with care. The bodies are forwarded to the
//! API, which validates them — the MCP server cannot bypass that. Note: minting API keys is
//! deliberately *not* exposed here, to avoid leaking secrets into an agent's context.

use serde_json::{json, Value};

use crate::client::Client;

const NAMES: &[&str] = &[
    "enqueue_benchmark",
    "create_project",
    "create_dataset",
    "add_dataset_item",
    "freeze_dataset",
    "create_rubric",
    "create_benchmark",
    "create_limit",
    "put_price",
];

/// True if `name` is a write tool — lets the registry give a precise "writes disabled" error.
pub(crate) fn is_write_tool(name: &str) -> bool {
    NAMES.contains(&name)
}

pub(crate) fn tools() -> Vec<Value> {
    vec![
        wtool("enqueue_benchmark",
            "Queue a benchmark run (non-blocking; `lt-runner serve` executes it). Returns the job — poll it with get_job.",
            json!({"type":"object","properties":{
                "benchmark":{"type":"string","description":"benchmark id"},
                "samples":{"type":"integer","description":"runs per case (default 1)"},
                "heal":{"type":"boolean","description":"attempt prompt healing on low scores (default false)"}
            },"required":["benchmark"]}),
            false),
        wtool("create_project",
            "Create a project.",
            json!({"type":"object","properties":{
                "name":{"type":"string"},
                "redaction":{"type":"string","enum":["none","hash","drop"],"description":"payload persistence (default none)"}
            },"required":["name"]}),
            false),
        wtool("create_dataset",
            "Create a dataset in a project.",
            json!({"type":"object","properties":{
                "project":{"type":"string"},
                "name":{"type":"string"},
                "source":{"type":"string","description":"provenance label, e.g. manual or events:recent"}
            },"required":["project","name"]}),
            false),
        wtool("add_dataset_item",
            "Append a case to a (non-frozen) dataset.",
            json!({"type":"object","properties":{
                "dataset":{"type":"string"},
                "input":{"type":"string","description":"the prompt / case input"},
                "output":{"type":"string","description":"a captured/candidate response"},
                "expected":{"type":"string","description":"golden reference answer"},
                "context":{"type":"string"},
                "tags":{"type":"array","items":{"type":"string"}}
            },"required":["dataset","input"]}),
            false),
        wtool("freeze_dataset",
            "Freeze a dataset so it becomes immutable and runs stay comparable. Idempotent.",
            json!({"type":"object","properties":{"dataset":{"type":"string"}},"required":["dataset"]}),
            true),
        wtool("create_rubric",
            "Create a structured, weighted rubric for per-dimension judging.",
            json!({"type":"object","properties":{
                "project":{"type":"string"},
                "name":{"type":"string"},
                "dimensions":{"type":"array","description":"[{key, description, weight?, anchors?:[string], floor?:number}]","items":{"type":"object"}},
                "threshold":{"type":"number","description":"overall pass threshold 0-1 (default 0.7)"}
            },"required":["project","name","dimensions"]}),
            false),
        wtool("create_benchmark",
            "Create a benchmark definition. Use `rubric` (freeform text) or `rubric_id` (structured). Supply an inline `dataset` or a `dataset_ref`; `targets` defines a multi-model comparison matrix.",
            json!({"type":"object","properties":{
                "project":{"type":"string"},
                "name":{"type":"string"},
                "rubric":{"type":"string","description":"freeform rubric text (single-score mode)"},
                "rubric_id":{"type":"string","description":"structured rubric id (per-dimension mode)"},
                "judge_model":{"type":"string","description":"[provider/]model, e.g. haiku or openai/gpt-4o-mini (default haiku)"},
                "dataset_ref":{"type":"string","description":"stored dataset id"},
                "dataset":{"type":"array","description":"inline cases [{input, expected?, ...}]","items":{"type":"object"}},
                "targets":{"type":"array","description":"comparison matrix [{provider, model, prompt?}]","items":{"type":"object"}},
                "baseline_score":{"type":"number"}
            },"required":["project","name"]}),
            false),
        wtool("create_limit",
            "Add a usage-limit rule to a project (applies to monitored ingest traffic only — the judge is exempt).",
            json!({"type":"object","properties":{
                "project":{"type":"string"},
                "metric":{"type":"string","enum":["cost_usd","calls","tokens"]},
                "window":{"type":"string","enum":["hour","day","month"]},
                "threshold":{"type":"number"},
                "action":{"type":"string","enum":["alert","throttle","block"],"description":"default alert"}
            },"required":["project","metric","window","threshold"]}),
            false),
        wtool("put_price",
            "Upsert a model's price (per-million-token rates); hot-swaps the live price book. Idempotent.",
            json!({"type":"object","properties":{
                "provider":{"type":"string"},
                "model":{"type":"string"},
                "input_per_mtok":{"type":"number"},
                "output_per_mtok":{"type":"number"},
                "cached_input_per_mtok":{"type":"number"},
                "source_url":{"type":"string"}
            },"required":["provider","model","input_per_mtok","output_per_mtok"]}),
            true),
    ]
}

fn wtool(name: &str, desc: &str, schema: Value, idempotent: bool) -> Value {
    json!({
        "name": name,
        "description": desc,
        "inputSchema": schema,
        "annotations": {
            "readOnlyHint": false,
            "destructiveHint": false,
            "idempotentHint": idempotent,
            "openWorldHint": true
        }
    })
}

/// Route a write tool. Returns `None` if `name` is not a write tool.
pub(crate) fn dispatch(c: &Client, name: &str, args: &Value) -> Option<Result<Value, String>> {
    let r = match name {
        "enqueue_benchmark" => match need(args, "benchmark") {
            Ok(b) => c.post(&format!("/v1/benchmarks/{b}/enqueue"), &pick(args, &["samples", "heal"])),
            Err(e) => Err(e),
        },
        "create_project" => post_with(c, args, &["name"], &["name", "redaction"], "/v1/projects".to_string()),
        "create_dataset" => match need(args, "project") {
            Ok(p) => post_with(c, args, &["name"], &["name", "source"], format!("/v1/projects/{p}/datasets")),
            Err(e) => Err(e),
        },
        "add_dataset_item" => match need(args, "dataset") {
            Ok(d) => post_with(
                c, args, &["input"],
                &["input", "output", "expected", "context", "tags"],
                format!("/v1/datasets/{d}/items"),
            ),
            Err(e) => Err(e),
        },
        "freeze_dataset" => match need(args, "dataset") {
            Ok(d) => c.post(&format!("/v1/datasets/{d}/freeze"), &json!({})),
            Err(e) => Err(e),
        },
        "create_rubric" => match need(args, "project") {
            Ok(p) => post_with(
                c, args, &["name", "dimensions"],
                &["name", "dimensions", "threshold"],
                format!("/v1/projects/{p}/rubrics"),
            ),
            Err(e) => Err(e),
        },
        "create_benchmark" => match need(args, "project") {
            Ok(p) => post_with(
                c, args, &["name"],
                &["name", "rubric", "rubric_id", "judge_model", "dataset_ref", "dataset", "targets", "baseline_score"],
                format!("/v1/projects/{p}/benchmarks"),
            ),
            Err(e) => Err(e),
        },
        "create_limit" => match need(args, "project") {
            Ok(p) => post_with(
                c, args, &["metric", "window", "threshold"],
                &["metric", "window", "threshold", "action"],
                format!("/v1/projects/{p}/limits"),
            ),
            Err(e) => Err(e),
        },
        "put_price" => match (need(args, "provider"), need(args, "model")) {
            (Ok(p), Ok(m)) => {
                let required = &["input_per_mtok", "output_per_mtok"];
                match missing(args, required) {
                    Some(e) => Err(e),
                    None => c.put(
                        &format!("/v1/prices/{p}/{m}"),
                        &pick(args, &["input_per_mtok", "output_per_mtok", "cached_input_per_mtok", "source_url"]),
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
fn post_with(c: &Client, args: &Value, required: &[&str], body_keys: &[&str], path: String) -> Result<Value, String> {
    match missing(args, required) {
        Some(e) => Err(e),
        None => c.post(&path, &pick(args, body_keys)),
    }
}

/// Require a string arg.
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
        .find(|k| args.get(**k).map_or(true, Value::is_null))
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
