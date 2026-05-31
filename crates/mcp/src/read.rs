//! Read-only tools — data gathering. Every tool here is side-effect-free and annotated
//! `readOnlyHint: true`, so a developer (or agent) can explore the whole system with no risk of
//! mutating state or affecting the running server.

use serde_json::{json, Value};

use crate::client::Client;

/// Tool definitions surfaced in `tools/list`.
pub(crate) fn tools() -> Vec<Value> {
    vec![
        tool("list_projects", "List all projects (admin key required in enforced mode).",
            json!({"type":"object","properties":{}})),
        tool("get_cost_summary", "Cost/usage rollup grouped by project + provider + model. Optionally filter by project.",
            json!({"type":"object","properties":{"project":{"type":"string"}}})),
        tool("query_events", "Recent LLM call events (newest first). Optionally filter by project.",
            json!({"type":"object","properties":{"project":{"type":"string"},"limit":{"type":"integer","description":"max events (default 20)"}}})),
        tool("get_event", "Fetch a single LLM call event by id.",
            json!({"type":"object","properties":{"event":{"type":"string","description":"event id"}},"required":["event"]})),
        tool("list_scores", "Recent LLM-as-judge scores (newest first). Optionally filter by project.",
            json!({"type":"object","properties":{"project":{"type":"string"},"limit":{"type":"integer","description":"max scores (default 20)"}}})),
        tool("get_limit_status", "Evaluate a project's limit rules now; per-rule status + overall throttle flag.",
            json!({"type":"object","properties":{"project":{"type":"string"}},"required":["project"]})),
        tool("list_limits", "List a project's configured limit rules.",
            json!({"type":"object","properties":{"project":{"type":"string"}},"required":["project"]})),
        tool("list_prices", "List the DB-backed model price book.",
            json!({"type":"object","properties":{}})),
        tool("list_benchmarks", "List a project's benchmark definitions (with inline datasets).",
            json!({"type":"object","properties":{"project":{"type":"string"}},"required":["project"]})),
        tool("get_benchmark", "Fetch one benchmark definition by id.",
            json!({"type":"object","properties":{"benchmark":{"type":"string"}},"required":["benchmark"]})),
        tool("get_benchmark_runs", "Run history (scorecards: mean score, pass rate, cost, status) for a benchmark.",
            json!({"type":"object","properties":{"benchmark":{"type":"string"}},"required":["benchmark"]})),
        tool("list_datasets", "List a project's datasets.",
            json!({"type":"object","properties":{"project":{"type":"string"}},"required":["project"]})),
        tool("get_dataset", "Fetch one dataset by id.",
            json!({"type":"object","properties":{"dataset":{"type":"string"}},"required":["dataset"]})),
        tool("list_dataset_items", "List the cases in a dataset.",
            json!({"type":"object","properties":{"dataset":{"type":"string"}},"required":["dataset"]})),
        tool("list_rubrics", "List a project's structured rubrics.",
            json!({"type":"object","properties":{"project":{"type":"string"}},"required":["project"]})),
        tool("get_rubric", "Fetch one rubric by id.",
            json!({"type":"object","properties":{"rubric":{"type":"string"}},"required":["rubric"]})),
        tool("list_jobs", "List background jobs (benchmark runs). Optionally filter by status.",
            json!({"type":"object","properties":{"status":{"type":"string","description":"queued|running|done|error"},"limit":{"type":"integer"}}})),
        tool("get_job", "Fetch one job by id — poll a benchmark run's status / progress / result.",
            json!({"type":"object","properties":{"job":{"type":"string"}},"required":["job"]})),
    ]
}

fn tool(name: &str, desc: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "description": desc,
        "inputSchema": schema,
        "annotations": { "readOnlyHint": true, "openWorldHint": true }
    })
}

/// Route a read tool. Returns `None` if `name` is not a read tool (so the caller can try writes).
pub(crate) fn dispatch(c: &Client, name: &str, args: &Value) -> Option<Result<Value, String>> {
    let r = match name {
        "list_projects" => c.get("/v1/projects"),
        "get_cost_summary" => c.get(&with_project("/v1/costs", args)),
        "query_events" => c.get(&list_path("/v1/events", args)),
        "get_event" => bind(args, "event", |id| c.get(&format!("/v1/events/{id}"))),
        "list_scores" => c.get(&list_path("/v1/scores", args)),
        "get_limit_status" => bind(args, "project", |p| c.get(&format!("/v1/limits/status?project={p}"))),
        "list_limits" => bind(args, "project", |p| c.get(&format!("/v1/projects/{p}/limits"))),
        "list_prices" => c.get("/v1/prices"),
        "list_benchmarks" => bind(args, "project", |p| c.get(&format!("/v1/projects/{p}/benchmarks"))),
        "get_benchmark" => bind(args, "benchmark", |b| c.get(&format!("/v1/benchmarks/{b}"))),
        "get_benchmark_runs" => bind(args, "benchmark", |b| c.get(&format!("/v1/benchmarks/{b}/runs"))),
        "list_datasets" => bind(args, "project", |p| c.get(&format!("/v1/projects/{p}/datasets"))),
        "get_dataset" => bind(args, "dataset", |d| c.get(&format!("/v1/datasets/{d}"))),
        "list_dataset_items" => bind(args, "dataset", |d| c.get(&format!("/v1/datasets/{d}/items"))),
        "list_rubrics" => bind(args, "project", |p| c.get(&format!("/v1/projects/{p}/rubrics"))),
        "get_rubric" => bind(args, "rubric", |r| c.get(&format!("/v1/rubrics/{r}"))),
        "list_jobs" => c.get(&jobs_path(args)),
        "get_job" => bind(args, "job", |j| c.get(&format!("/v1/jobs/{j}"))),
        _ => return None,
    };
    Some(r)
}

/// Extract a required string arg and run `f` with it, or return a clear error.
fn bind(args: &Value, key: &str, f: impl FnOnce(&str) -> Result<Value, String>) -> Result<Value, String> {
    match args.get(key).and_then(Value::as_str) {
        Some(v) => f(v),
        None => Err(format!("missing required argument: {key}")),
    }
}

fn with_project(base: &str, args: &Value) -> String {
    match args.get("project").and_then(Value::as_str) {
        Some(p) => format!("{base}?project={p}"),
        None => base.to_string(),
    }
}

fn list_path(base: &str, args: &Value) -> String {
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20);
    let mut p = format!("{base}?limit={limit}");
    if let Some(proj) = args.get("project").and_then(Value::as_str) {
        p.push_str(&format!("&project={proj}"));
    }
    p
}

fn jobs_path(args: &Value) -> String {
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20);
    let mut p = format!("/v1/jobs?limit={limit}");
    if let Some(s) = args.get("status").and_then(Value::as_str) {
        p.push_str(&format!("&status={s}"));
    }
    p
}
