//! Read-only tools — **dispatch only**. Every tool routed here is side-effect-free, and the
//! catalog that says so (names, descriptions, input schemas, `readOnlyHint`) is generated from
//! `lighttrack-contract`, so a tool cannot exist in the list and not here, or here and not in the
//! list. What stays is the part that is genuinely I/O: which URL each tool's arguments become.

use serde_json::Value;

use crate::client::Client;

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
        "list_price_history" => bind2(args, "provider", "model", |p, m| {
            c.get(&format!("/v1/prices/history/{p}/{m}"))
        }),
        "list_unpriced_models" => c.get(&unpriced_path(args)),
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
        "list_labels" => {
            let mut p = list_path("/v1/labels", args);
            push_str_params(&mut p, args, &["subject", "rubric_id", "cursor"]);
            c.get(&p)
        }
        "get_judge_trust" => bind(args, "judge", |j| {
            let mut p = format!("/v1/judges/trust?judge={j}");
            push_str_params(&mut p, args, &["project", "rubric_id"]);
            c.get(&p)
        }),
        "list_calibrations" => {
            let mut p = list_path("/v1/calibrations", args);
            push_str_params(&mut p, args, &["cursor"]);
            c.get(&p)
        }
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
        "get_collective_contributions" => contributions_path(args),
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

/// [`bind`] for a tool that needs two required arguments (a `(provider, model)` key).
fn bind2(
    args: &Value,
    a: &str,
    b: &str,
    f: impl FnOnce(&str, &str) -> Result<Value, String>,
) -> Result<Value, String> {
    match (
        args.get(a).and_then(Value::as_str),
        args.get(b).and_then(Value::as_str),
    ) {
        (Some(x), Some(y)) => f(x, y),
        (None, _) => Err(format!("missing required argument: {a}")),
        (_, None) => Err(format!("missing required argument: {b}")),
    }
}

/// `/v1/costs/unpriced`, with the optional narrowing the caller gave. Both parameters are optional:
/// the default (every project this key can read, last 30 days) is the question an agent asks first.
fn unpriced_path(args: &Value) -> String {
    let mut p = "/v1/costs/unpriced".to_string();
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

/// The ledger page. Both parameters are optional and the server decides the default page size, so
/// an agent that passes nothing still gets a sane page rather than the whole table.
fn contributions_path(args: &Value) -> String {
    let mut p = "/v1/collective/contributions".to_string();
    let mut sep = '?';
    if let Some(n) = args.get("limit").and_then(Value::as_u64) {
        p.push_str(&format!("{sep}limit={n}"));
        sep = '&';
    }
    if let Some(c) = args
        .get("cursor")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
    {
        p.push_str(&format!("{sep}cursor={c}"));
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
        let t = crate::tools::list(false)["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .find(|t| t["name"] == "get_capabilities")
            .cloned()
            .expect("get_capabilities is listed");
        assert_eq!(t["annotations"]["readOnlyHint"], true);
        assert_eq!(
            t["outputSchema"]["properties"]["unsupported"]["type"], "array",
            "the refused surfaces are part of the declared contract"
        );
    }

    /// The unpriced ledger is a read like any other, and its default — no project, no window — has
    /// to be the bare route: an agent asking "what are we failing to price" should not have to know
    /// a window to get an answer.
    #[test]
    fn the_unpriced_ledger_is_a_read_only_tool_with_no_required_arguments() {
        let t = crate::tools::list(false)["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .find(|t| t["name"] == "list_unpriced_models")
            .cloned()
            .expect("list_unpriced_models is listed");
        assert_eq!(t["annotations"]["readOnlyHint"], true);
        assert!(
            t["inputSchema"].get("required").is_none(),
            "no argument is required"
        );
        assert_eq!(
            t["outputSchema"]["properties"]["price_book"]["properties"]["stale"]["type"], "boolean",
            "book staleness is part of the declared contract, not an incidental field"
        );
        assert_eq!(unpriced_path(&json!({})), "/v1/costs/unpriced");
        assert_eq!(
            unpriced_path(&json!({ "project": "p1", "since": "2026-01-01T00:00:00Z" })),
            "/v1/costs/unpriced?project=p1&since=2026-01-01T00:00:00Z"
        );
        // A blank value must not become a `?project=` that scopes the read to nothing.
        assert_eq!(
            unpriced_path(&json!({ "project": "" })),
            "/v1/costs/unpriced"
        );
    }

    /// Both halves of the price key are required — a history call missing one must name which.
    #[test]
    fn the_price_history_tool_names_the_argument_it_is_missing() {
        let err = |args: Value| {
            bind2(&args, "provider", "model", |_, _| Ok(Value::Null)).expect_err("should refuse")
        };
        assert!(err(json!({})).contains("provider"));
        assert!(err(json!({ "provider": "openai" })).contains("model"));
        assert!(bind2(
            &json!({ "provider": "openai", "model": "gpt-4o" }),
            "provider",
            "model",
            |p, m| { Ok(json!(format!("{p}/{m}"))) }
        )
        .is_ok());
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
    fn contributions_path_is_bare_by_default_and_pages_on_request() {
        assert_eq!(
            contributions_path(&json!({})),
            "/v1/collective/contributions",
            "an agent that passes nothing gets the server's default page, not the whole table"
        );
        assert_eq!(
            contributions_path(&json!({ "limit": 5 })),
            "/v1/collective/contributions?limit=5"
        );
        assert_eq!(
            contributions_path(&json!({ "limit": 5, "cursor": "abc" })),
            "/v1/collective/contributions?limit=5&cursor=abc"
        );
        // A cursor alone must still open the query with `?`, not `&`.
        assert_eq!(
            contributions_path(&json!({ "cursor": "abc" })),
            "/v1/collective/contributions?cursor=abc"
        );
        assert_eq!(
            contributions_path(&json!({ "cursor": "" })),
            "/v1/collective/contributions"
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
        let names: Vec<String> = crate::tools::list(false)["tools"]
            .as_array()
            .expect("tools array")
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
