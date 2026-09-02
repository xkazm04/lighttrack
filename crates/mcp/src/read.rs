//! Read-only tools — data gathering. Every tool here is side-effect-free and annotated
//! `readOnlyHint: true`, so a developer (or agent) can explore the whole system with no risk of
//! mutating state or affecting the running server.

use serde_json::{json, Value};

use crate::client::Client;

/// Tool definitions surfaced in `tools/list`.
pub(crate) fn tools() -> Vec<Value> {
    vec![
        tool("get_capabilities", "What this LightTrack deployment's store backend actually serves: the backend name, the surfaces it implements, the surfaces it REFUSES (whose routes answer HTTP 501 `unsupported` rather than an empty result), and whether usage caps are enforced atomically or are merely advisory. Read this before concluding a surface returned no data — a 501 here means 'not ported on this backend', never 'you have none'.",
            json!({"type":"object","properties":{}})),
        tool("list_projects", "List all projects (admin key required in enforced mode).",
            json!({"type":"object","properties":{}})),
        tool("get_cost_summary", "Cost/usage rollup grouped by project + provider + model. Optionally filter by project.",
            json!({"type":"object","properties":{"project":{"type":"string"}}})),
        tool("get_margin", "Profit rollup: revenue − LLM cost grouped by customer or product over a window (default last 30 days). Most-unprofitable first.",
            json!({"type":"object","properties":{
                "by":{"type":"string","enum":["customer","product"],"description":"group dimension (default customer)"},
                "project":{"type":"string"},
                "since":{"type":"string","description":"RFC3339 window start (default 30d ago)"},
                "until":{"type":"string","description":"RFC3339 window end (default now)"}
            }})),
        tool("get_forecast", "Predictive cost/margin forecast for a project: projected spend, per-budget breach ETAs (\"will breach in ~N days\"), per-customer/product margin-erosion crossovers (\"turns unprofitable next week\"), and the pre-emptive alerts derived from them. Fits an EWMA/linear trend over the recent daily counters.",
            json!({"type":"object","properties":{
                "project":{"type":"string"},
                "by":{"type":"string","enum":["customer","product"],"description":"margin dimension (default customer)"},
                "horizon":{"type":"integer","description":"days to project ahead (default 14, 1..=90)"},
                "lookback":{"type":"integer","description":"trailing days of history to fit (default 14, 2..=90)"}
            },"required":["project"]})),
        tool("query_events", "Recent LLM call events (newest first). Filter by project/time window/provider/model/trace/use-case name; page with `cursor` (from a prior call's next_cursor).",
            json!({"type":"object","properties":{
                "project":{"type":"string"},
                "limit":{"type":"integer","description":"max events (default 20, max 1000)"},
                "since":{"type":"string","description":"RFC3339 lower bound on event time (inclusive)"},
                "until":{"type":"string","description":"RFC3339 upper bound on event time (exclusive)"},
                "provider":{"type":"string","description":"exact provider match (anthropic, openai, …)"},
                "model":{"type":"string","description":"exact model match"},
                "trace_id":{"type":"string","description":"only events in this trace"},
                "name":{"type":"string","description":"use-case name filter (a call's `name`)"},
                "cursor":{"type":"string","description":"keyset cursor from a prior call's next_cursor"}
            }})),
        tool("get_event", "Fetch a single LLM call event by id.",
            json!({"type":"object","properties":{"event":{"type":"string","description":"event id"}},"required":["event"]})),
        tool("list_traces", "Recent agent traces (events grouped by trace_id), newest first — end-to-end cost, latency, tokens, and span count per request. Filter by project/time window/status/min cost; page with `cursor`.",
            json!({"type":"object","properties":{
                "project":{"type":"string"},
                "limit":{"type":"integer","description":"max traces (default 20, max 1000)"},
                "since":{"type":"string","description":"RFC3339 lower bound on the trace's end time (inclusive)"},
                "until":{"type":"string","description":"RFC3339 upper bound on the trace's end time (exclusive)"},
                "status":{"type":"string","enum":["success","error"],"description":"keep only traces of this status"},
                "min_cost":{"type":"number","description":"minimum whole-trace cost (USD)"},
                "cursor":{"type":"string","description":"keyset cursor from a prior call's next_cursor"}
            }})),
        tool("get_trace", "Fetch one trace by id: rolled-up totals, the span tree, and any scores recorded within it.",
            json!({"type":"object","properties":{"trace":{"type":"string","description":"trace id"}},"required":["trace"]})),
        tool("list_scores", "Recent LLM-as-judge scores (newest first). Optionally narrowed to one project, one rubric (`rubric_id`), or one kind of verdict (`kind`) - a benchmark case is not the same measurement as an ad-hoc score, and averaging them together is the mistake this filter exists to prevent.",
            json!({"type":"object","properties":{
                "project":{"type":"string"},
                "rubric_id":{"type":"string","description":"only verdicts judged against this stored rubric; survives a rubric rename, unlike the free-text `rubric` label"},
                "kind":{"type":"string","enum":["freeform","rubric","bench_case","compare_cell","pairwise_game","calibration","trace"],"description":"only verdicts of this kind"},
                "limit":{"type":"integer","description":"max scores (default 20)"}
            }})),
        tool("get_limit_status", "Evaluate a project's limit rules now; per-rule status + overall throttle flag.",
            json!({"type":"object","properties":{"project":{"type":"string"}},"required":["project"]})),
        tool("list_limits", "List a project's configured limit rules.",
            json!({"type":"object","properties":{"project":{"type":"string"}},"required":["project"]})),
        tool("list_alerts", "The fired-alert ledger: what LightTrack has actually alerted on (limit breaches, spend forecasts, error spikes, quality regressions, dead relay tasks, finished benchmark runs, rejected ingest), whether each delivery LANDED, who acknowledged it, and what came of it. This is the durable record — an alert that fired while nobody was watching is here, and `delivered: []` means the alert reached no channel at all.",
            json!({"type":"object","properties":{
                "project":{"type":"string"},
                "kind":{"type":"string","enum":["limit_breach","limit_warning","forecast_alert","relay_task_dead","error_spike","score_drop","bench_run","ingest_rejected"],"description":"only alerts of this kind"},
                "since":{"type":"string","description":"window start: an RFC3339 instant, or a relative 30m / 24h / 7d"},
                "acked":{"type":"boolean","description":"true = only acknowledged, false = only open (the on-call view); omit for both"},
                "limit":{"type":"integer","description":"max alerts (default 20)"},
                "cursor":{"type":"string","description":"keyset cursor from a prior call's next_cursor"}
            }})),
        tool("list_margin_policies", "List a project's standing margin guardrails: the policies that turn a loss-making or eroding customer into a limit rule automatically. Read-only — the rules they create show up in `list_limits` carrying an `origin`.",
            json!({"type":"object","properties":{"project":{"type":"string"}},"required":["project"]})),
        tool("list_prices", "List the DB-backed model price book.",
            json!({"type":"object","properties":{}})),
        tool("list_benchmarks", "List a project's benchmark definitions (with inline datasets).",
            json!({"type":"object","properties":{"project":{"type":"string"}},"required":["project"]})),
        tool("get_benchmark", "Fetch one benchmark definition by id.",
            json!({"type":"object","properties":{"benchmark":{"type":"string"}},"required":["benchmark"]})),
        tool("get_benchmark_runs", "Run history (scorecards: mean score, pass rate, cost, status) for a benchmark.",
            json!({"type":"object","properties":{"benchmark":{"type":"string"}},"required":["benchmark"]})),
        tool("check_benchmark_gate", "CI-gate verdict for a benchmark from its latest finished run: pass | regressed | no_baseline | no_runs, with the supporting run id, mean, baseline, and case count. Use in a pipeline step to block a regression.",
            json!({"type":"object","properties":{"benchmark":{"type":"string","description":"benchmark id"}},"required":["benchmark"]})),
        tool("query_rollup", "THE grouped cost/usage question: totals over a window, grouped by 1-3 of project/provider/model/name/api_key/customer/product/prompt/day, with optional equality filters. Every fixed cost surface (costs, usecases, margin, forecast) is one grouping of this — use it for anything they do not already answer, e.g. \"cost per customer per day\" or \"which model drives this product's spend\". Rows carry `unpriced_calls`: when it is non-zero the cost is a FLOOR, not a total, because those calls had no price in the book.",
            json!({"type":"object","properties":{
                "project":{"type":"string"},
                "by":{"type":"string","description":"comma-separated dimensions, 1..=3 (default provider,model): project|provider|model|name|api_key|customer|product|prompt|day"},
                "since":{"type":"string","description":"RFC3339 window start (default 30d ago)"},
                "until":{"type":"string","description":"RFC3339 window end (exclusive)"},
                "time":{"type":"string","enum":["ts","received_at"],"description":"which timestamp the window and `day` bucket read (default ts; accounting reads use received_at)"},
                "filter":{"type":"string","description":"comma-separated `dimension:value` equality predicates, e.g. customer:acme,model:gpt-5.4"}
            }})),
        tool("get_usecases", "Use-case cost rollup: usage + cost grouped by (name, provider, model) for a project, optionally windowed from `since`. A call's use-case is its `name`, or its model when unnamed.",
            json!({"type":"object","properties":{
                "project":{"type":"string"},
                "since":{"type":"string","description":"RFC3339 window start (inclusive); omit for full history"}
            },"required":["project"]})),
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
        tool("list_schedules", "List recurring workloads: every stored schedule (a job kind + payload on an interval), for one project or the whole deployment. This is the answer to \"what runs on a schedule here\" — including recurring compare benchmarks, which cannot express recurrence any other way.",
            json!({"type":"object","properties":{"project":{"type":"string","description":"one project's schedules; omit for every project's"}}})),
        tool("get_collective_leaderboard", "The collective real-world model leaderboard: quality × cost × latency per (provider, model, task type), merged across contributing LightTrack instances. Optionally filter by task_type or provider.",
            json!({"type":"object","properties":{
                "task_type":{"type":"string","description":"filter to one task bucket (qa, summarization, coding, …)"},
                "provider":{"type":"string","description":"filter to one provider (anthropic, openai, …)"},
                "determinism":{"type":"string","description":"rigor filter: keep only rows where EVERY source ran at this determinism level (exact|best-effort|sampled)"},
                "frozen_dataset":{"type":"string","description":"rigor filter: 'true' keeps only rows whose every source ran against a frozen, single-version dataset"},
                "significance_tested":{"type":"string","description":"rigor filter: 'true' keeps only rows whose every source's verdict was significance-tested"}
            }})),
        tool("get_collective_digest", "This instance's privacy-safe, k-anonymized model digest — the aggregate scorecards it would contribute to a collective hub (admin key required). Never reads raw events.",
            json!({"type":"object","properties":{
                "min_cases":{"type":"integer","description":"k-anonymity floor: only (model, task) buckets with ≥ this many cases are published (default from server)"}
            }})),
    ]
}

fn tool(name: &str, desc: &str, schema: Value) -> Value {
    let mut t = json!({
        "name": name,
        "description": desc,
        "inputSchema": schema,
        "annotations": { "readOnlyHint": true, "openWorldHint": true }
    });
    // Tools that return rendered data also advertise the shape of their `structuredContent`.
    if let Some(out) = crate::schemas::output_schema(name) {
        if let Some(obj) = t.as_object_mut() {
            obj.insert("outputSchema".to_string(), out);
        }
    }
    t
}

/// Route a read tool. Returns `None` if `name` is not a read tool (so the caller can try writes).
pub(crate) fn dispatch(c: &Client, name: &str, args: &Value) -> Option<Result<Value, String>> {
    let r = match name {
        "get_capabilities" => c.get("/v1/capabilities"),
        "list_projects" => c.get("/v1/projects"),
        "get_cost_summary" => c.get(&with_project("/v1/costs", args)),
        "get_margin" => c.get(&margin_path(args)),
        "get_forecast" => c.get(&forecast_path(args)),
        "get_event" => bind(args, "event", |id| c.get(&format!("/v1/events/{id}"))),
        "get_trace" => bind(args, "trace", |id| c.get(&format!("/v1/traces/{id}"))),
        "list_scores" => {
            let mut p = list_path("/v1/scores", args);
            push_str_params(&mut p, args, &["rubric_id", "kind"]);
            c.get(&p)
        }
        "get_limit_status" => bind(args, "project", |p| {
            c.get(&format!("/v1/limits/status?project={p}"))
        }),
        "list_limits" => bind(args, "project", |p| {
            c.get(&format!("/v1/projects/{p}/limits"))
        }),
        "list_alerts" => c.get(&alerts_path(args)),
        "list_margin_policies" => bind(args, "project", |p| {
            c.get(&format!("/v1/projects/{p}/margin-policies"))
        }),
        "list_prices" => c.get("/v1/prices"),
        "list_benchmarks" => bind(args, "project", |p| {
            c.get(&format!("/v1/projects/{p}/benchmarks"))
        }),
        "get_benchmark" => bind(args, "benchmark", |b| c.get(&format!("/v1/benchmarks/{b}"))),
        "get_benchmark_runs" => bind(args, "benchmark", |b| {
            c.get(&format!("/v1/benchmarks/{b}/runs"))
        }),
        "check_benchmark_gate" => bind(args, "benchmark", |b| {
            c.get(&format!("/v1/benchmarks/{b}/gate"))
        }),
        "get_usecases" => c.get(&usecases_path(args)),
        "query_rollup" => c.get(&rollup_path(args)),
        "list_datasets" => bind(args, "project", |p| {
            c.get(&format!("/v1/projects/{p}/datasets"))
        }),
        "get_dataset" => bind(args, "dataset", |d| c.get(&format!("/v1/datasets/{d}"))),
        "list_dataset_items" => bind(args, "dataset", |d| {
            c.get(&format!("/v1/datasets/{d}/items"))
        }),
        "list_rubrics" => bind(args, "project", |p| {
            c.get(&format!("/v1/projects/{p}/rubrics"))
        }),
        "get_rubric" => bind(args, "rubric", |r| c.get(&format!("/v1/rubrics/{r}"))),
        "list_jobs" => c.get(&jobs_path(args)),
        "get_job" => bind(args, "job", |j| c.get(&format!("/v1/jobs/{j}"))),
        "list_schedules" => c.get(&match args.get("project").and_then(Value::as_str) {
            Some(p) => format!("/v1/projects/{p}/schedules"),
            None => "/v1/schedules".to_string(),
        }),
        "get_collective_leaderboard" => c.get(&collective_path(args)),
        "get_collective_digest" => c.get(&collective_digest_path(args)),
        _ => return None,
    };
    Some(r)
}

/// Route a paged read tool (keyset cursor returned in the response header). Returns `None` for tools
/// that aren't paged, so `tools::call` falls back to the plain `dispatch`.
pub(crate) fn dispatch_paged(
    c: &Client,
    name: &str,
    args: &Value,
) -> Option<Result<(Value, Option<String>), String>> {
    let path = match name {
        "query_events" => events_path(args),
        "list_traces" => traces_path(args),
        _ => return None,
    };
    Some(c.get_paged(&path))
}

/// Extract a required string arg and run `f` with it, or return a clear error.
fn bind(
    args: &Value,
    key: &str,
    f: impl FnOnce(&str) -> Result<Value, String>,
) -> Result<Value, String> {
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

/// Append `&key=value` for each present, non-empty string arg in `keys`. Cursors are opaque hex and the
/// other values are ids/enums/timestamps, so no percent-encoding is needed (matching the rest of the
/// client, which interpolates query values directly).
fn push_str_params(p: &mut String, args: &Value, keys: &[&str]) {
    for k in keys {
        if let Some(v) = args
            .get(*k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            p.push_str(&format!("&{k}={v}"));
        }
    }
}

/// `GET /v1/alerts` with its filter set. `acked` is a genuine tri-state — omitting it must send no
/// `acked=` at all, because `acked=false` is "open only", not "no filter".
fn alerts_path(args: &Value) -> String {
    let mut p = list_path("/v1/alerts", args);
    push_str_params(&mut p, args, &["kind", "since", "cursor"]);
    if let Some(a) = args.get("acked").and_then(Value::as_bool) {
        p.push_str(&format!("&acked={a}"));
    }
    p
}

/// `GET /v1/events` with its full filter + keyset-cursor set (see `get_events` in the API).
fn events_path(args: &Value) -> String {
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20);
    let mut p = format!("/v1/events?limit={limit}");
    push_str_params(
        &mut p,
        args,
        &[
            "project", "since", "until", "provider", "model", "trace_id", "name", "cursor",
        ],
    );
    p
}

/// `GET /v1/traces` with its window/status/min_cost filters + keyset cursor (see `list_traces`).
fn traces_path(args: &Value) -> String {
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20);
    let mut p = format!("/v1/traces?limit={limit}");
    push_str_params(
        &mut p,
        args,
        &["project", "since", "until", "status", "cursor"],
    );
    if let Some(mc) = args.get("min_cost").and_then(Value::as_f64) {
        p.push_str(&format!("&min_cost={mc}"));
    }
    p
}

/// `/v1/rollup` with only the args the caller actually supplied — the API's own defaults
/// (30-day window, `provider,model` grouping) are the ones an agent should get when it omits them,
/// rather than a second set of defaults invented here.
fn rollup_path(args: &Value) -> String {
    let mut p = "/v1/rollup".to_string();
    let mut sep = '?';
    for k in ["project", "by", "since", "until", "time", "filter"] {
        if let Some(v) = args
            .get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            p.push_str(&format!("{sep}{k}={v}"));
            sep = '&';
        }
    }
    p
}

fn margin_path(args: &Value) -> String {
    let by = args.get("by").and_then(Value::as_str).unwrap_or("customer");
    let mut p = format!("/v1/margin?by={by}");
    for k in ["project", "since", "until"] {
        if let Some(v) = args.get(k).and_then(Value::as_str) {
            p.push_str(&format!("&{k}={v}"));
        }
    }
    p
}

fn forecast_path(args: &Value) -> String {
    let mut p = "/v1/forecast".to_string();
    let mut sep = '?';
    for k in ["project", "by"] {
        if let Some(v) = args.get(k).and_then(Value::as_str) {
            p.push_str(&format!("{sep}{k}={v}"));
            sep = '&';
        }
    }
    for k in ["horizon", "lookback"] {
        if let Some(v) = args.get(k).and_then(Value::as_u64) {
            p.push_str(&format!("{sep}{k}={v}"));
            sep = '&';
        }
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

fn collective_path(args: &Value) -> String {
    let mut p = "/v1/collective/leaderboard".to_string();
    let mut sep = '?';
    for k in [
        "task_type",
        "provider",
        "determinism",
        "frozen_dataset",
        "significance_tested",
    ] {
        if let Some(v) = args.get(k).and_then(Value::as_str) {
            p.push_str(&format!("{sep}{k}={v}"));
            sep = '&';
        }
    }
    p
}

fn collective_digest_path(args: &Value) -> String {
    match args.get("min_cases").and_then(Value::as_u64) {
        Some(n) => format!("/v1/collective/digest?min_cases={n}"),
        None => "/v1/collective/digest".to_string(),
    }
}

/// `GET /v1/usecases` — required `project`, optional `since` window.
fn usecases_path(args: &Value) -> String {
    let mut p = "/v1/usecases".to_string();
    let mut sep = '?';
    for k in ["project", "since"] {
        if let Some(v) = args
            .get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            p.push_str(&format!("{sep}{k}={v}"));
            sep = '&';
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The manifest tool is a read: it must be listed, annotated `readOnlyHint`, and declare the
    /// output shape an agent branches on (`unsupported`).
    #[test]
    fn get_capabilities_is_a_listed_read_only_tool() {
        let t = tools()
            .into_iter()
            .find(|t| t["name"] == "get_capabilities")
            .expect("get_capabilities is listed");
        assert_eq!(t["annotations"]["readOnlyHint"], true);
        assert_eq!(
            t["outputSchema"]["properties"]["unsupported"]["type"], "array",
            "the refused surfaces are part of the declared contract"
        );
    }

    #[test]
    fn events_path_defaults_to_limit_only() {
        assert_eq!(events_path(&json!({})), "/v1/events?limit=20");
    }

    #[test]
    fn events_path_assembles_all_filters_and_cursor() {
        let p = events_path(&json!({
            "limit": 100, "project": "p1", "since": "2026-01-01T00:00:00Z",
            "until": "2026-02-01T00:00:00Z", "provider": "openai", "model": "gpt-4o",
            "trace_id": "t-9", "name": "summarize", "cursor": "deadbeef"
        }));
        assert!(p.starts_with("/v1/events?limit=100"));
        for frag in [
            "&project=p1",
            "&since=2026-01-01T00:00:00Z",
            "&until=2026-02-01T00:00:00Z",
            "&provider=openai",
            "&model=gpt-4o",
            "&trace_id=t-9",
            "&name=summarize",
            "&cursor=deadbeef",
        ] {
            assert!(p.contains(frag), "missing {frag} in {p}");
        }
    }

    #[test]
    fn events_path_skips_empty_and_absent() {
        let p = events_path(&json!({ "project": "", "provider": "anthropic" }));
        assert_eq!(p, "/v1/events?limit=20&provider=anthropic");
    }

    #[test]
    fn traces_path_includes_status_and_numeric_min_cost() {
        let p = traces_path(&json!({
            "project": "p1", "status": "error", "min_cost": 0.5, "cursor": "abcd"
        }));
        assert!(p.starts_with("/v1/traces?limit=20"));
        assert!(p.contains("&project=p1"));
        assert!(p.contains("&status=error"));
        assert!(p.contains("&min_cost=0.5"));
        assert!(p.contains("&cursor=abcd"));
    }

    #[test]
    fn usecases_path_requires_project_and_optional_since() {
        assert_eq!(
            usecases_path(&json!({ "project": "p1" })),
            "/v1/usecases?project=p1"
        );
        assert_eq!(
            usecases_path(&json!({ "project": "p1", "since": "2026-01-01T00:00:00Z" })),
            "/v1/usecases?project=p1&since=2026-01-01T00:00:00Z"
        );
    }

    /// Only what the caller supplied reaches the query string, so the API's defaults apply to the
    /// rest. A path that always pinned `by=` would silently answer a different question than the
    /// one an agent asked with no grouping.
    #[test]
    fn rollup_path_passes_only_the_supplied_args() {
        assert_eq!(rollup_path(&json!({})), "/v1/rollup");
        assert_eq!(
            rollup_path(&json!({ "project": "p1", "by": "customer,day" })),
            "/v1/rollup?project=p1&by=customer,day"
        );
        let p = rollup_path(&json!({
            "by": "model", "time": "received_at", "filter": "customer:acme", "since": ""
        }));
        assert!(p.starts_with("/v1/rollup?by=model"), "{p}");
        assert!(
            p.contains("&time=received_at") && p.contains("&filter=customer:acme"),
            "{p}"
        );
        assert!(
            !p.contains("since"),
            "an empty arg is omitted, not sent blank: {p}"
        );
    }

    #[test]
    fn collective_digest_path_passes_min_cases() {
        assert_eq!(collective_digest_path(&json!({})), "/v1/collective/digest");
        assert_eq!(
            collective_digest_path(&json!({ "min_cases": 5 })),
            "/v1/collective/digest?min_cases=5"
        );
    }

    #[test]
    fn new_read_tools_are_registered_with_schemas() {
        let names: Vec<String> = tools()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        for n in [
            "check_benchmark_gate",
            "get_usecases",
            "get_collective_digest",
        ] {
            assert!(names.contains(&n.to_string()), "{n} missing");
        }
    }

    #[test]
    fn dispatch_paged_only_matches_paged_tools() {
        // A trivial client is never actually called for the non-paged branch (returns None first).
        let c = Client::from_env();
        assert!(dispatch_paged(&c, "list_scores", &json!({})).is_none());
        assert!(dispatch_paged(&c, "get_event", &json!({})).is_none());
    }
}
