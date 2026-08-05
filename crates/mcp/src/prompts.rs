//! MCP Prompts — pre-built, read-only journeys that Claude Code surfaces as slash commands
//! (`/lighttrack:cost-report`, `/lighttrack:benchmark-leaderboard`, …).
//!
//! Each `prompts/get` returns a single user-role instruction that tells the agent *which* read tools
//! to call and *how* to present the result — the render layer already formats the tables, so the
//! prompts just steer tool choice + framing. They never trigger writes. Missing-argument cases
//! degrade gracefully (call `list_projects` first, or ask) rather than erroring.

use serde_json::{json, Value};

/// `prompts/list` payload.
pub(crate) fn list() -> Value {
    json!({
        "prompts": [
            def("cost-report", "Cost rollup for a project (or all projects), with limit warnings.",
                &[arg("project", "project id (omit for all projects)", false)]),
            def("limit-check", "Evaluate a project's usage limits right now.",
                &[arg("project", "project id", true)]),
            def("benchmark-leaderboard", "Run history + regression check for a benchmark.",
                &[arg("benchmark", "benchmark id", true)]),
            def("score-triage", "Recent judge scores; surface and drill into failures.",
                &[arg("project", "project id (optional)", false), arg("limit", "max scores", false)]),
            def("recent-activity", "Recent LLM calls + spend for a project (or all).",
                &[arg("project", "project id (optional)", false), arg("limit", "max events", false)]),
            def("price-book", "Show the model price book with cheapest/priciest call-outs.", &[]),
            def("margin-report", "Profit per customer/product — net revenue against LLM cost, money-losers first.",
                &[arg("by", "customer|product (default customer)", false), arg("project", "project id (optional)", false)]),
        ]
    })
}

/// Handle `prompts/get`: resolve the named prompt + its arguments into a user message.
pub(crate) fn get(params: &Value) -> Result<Value, String> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let a = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };

    let (desc, text) = match name {
        "cost-report" => cost_report(a("project")),
        "limit-check" => limit_check(a("project")),
        "benchmark-leaderboard" => benchmark_leaderboard(a("benchmark")),
        "score-triage" => score_triage(a("project"), a("limit")),
        "recent-activity" => recent_activity(a("project"), a("limit")),
        "price-book" => price_book(),
        "margin-report" => margin_report(a("by"), a("project")),
        _ => return Err(format!("unknown prompt: {name}")),
    };
    Ok(json!({
        "description": desc,
        "messages": [ { "role": "user", "content": { "type": "text", "text": text } } ]
    }))
}

fn def(name: &str, desc: &str, args: &[Value]) -> Value {
    json!({ "name": name, "description": desc, "arguments": args })
}

fn arg(name: &str, desc: &str, required: bool) -> Value {
    json!({ "name": name, "description": desc, "required": required })
}

fn cost_report(project: Option<&str>) -> (&'static str, String) {
    let text = match project {
        Some(p) => format!(
            "Produce a LightTrack cost report for project `{p}`.\n\n\
             1. Call `get_cost_summary` with project `{p}` and present the returned table verbatim (it is already formatted).\n\
             2. Call `get_limit_status` for `{p}`; call out any rule that is breached or at ≥80% of its threshold, naming the metric and window.\n\
             3. Close with the single biggest cost driver (provider/model) and, if one model dominates spend, one concrete cost-saving suggestion."
        ),
        None =>
            "Produce a LightTrack cost report across all projects.\n\n\
             1. Call `get_cost_summary` (no project filter) and present the returned table verbatim.\n\
             2. Identify the top-spending project, call `get_limit_status` for it, and flag any rule breached or at ≥80% of its threshold.\n\
             3. Close with the biggest cost driver (project + provider/model) and one cost-saving suggestion if a single model dominates spend.".to_string(),
    };
    ("LightTrack cost report with limit warnings", text)
}

fn limit_check(project: Option<&str>) -> (&'static str, String) {
    let text = match project {
        Some(p) => format!(
            "Check usage limits for project `{p}`.\n\n\
             Call `get_limit_status` with project `{p}` and present the table. If the project is throttled, or any rule is breached or near (≥80%), state plainly what is at risk — which metric, which window, and how far over. Otherwise confirm it is comfortably within limits."
        ),
        None =>
            "Check usage limits.\n\n\
             No project was given: first call `list_projects`, then call `get_limit_status` for each (or ask the user which project to check). Present each table and flag any project that is throttled, breached, or near (≥80%).".to_string(),
    };
    ("Evaluate a project's usage limits", text)
}

fn benchmark_leaderboard(benchmark: Option<&str>) -> (&'static str, String) {
    let text = match benchmark {
        Some(b) => format!(
            "Show the run history for benchmark `{b}`.\n\n\
             1. Call `get_benchmark_runs` with benchmark `{b}` and present the leaderboard table and mean-score trend exactly as returned.\n\
             2. State whether the latest run regressed versus the prior run, and note any notable change in p50 latency or cost.\n\
             3. If you need the baseline or rubric to judge the regression, call `get_benchmark` for `{b}`."
        ),
        None =>
            "Show a benchmark's run history.\n\n\
             No benchmark id was given: ask the user which benchmark, or call `list_benchmarks` for a project first, then `get_benchmark_runs` for the chosen id. Present the leaderboard and call out any regression.".to_string(),
    };
    ("Benchmark run history + regression check", text)
}

fn score_triage(project: Option<&str>, limit: Option<&str>) -> (&'static str, String) {
    let scope = project
        .map(|p| format!(" for project `{p}`"))
        .unwrap_or_default();
    let call = call_with("list_scores", project, limit, "limit");
    let text = format!(
        "Triage recent LLM-as-judge scores{scope}.\n\n\
         1. Call {call} and present the table with its mean and trend.\n\
         2. List the failing scores (Pass = ❌) with their rubric and judge model.\n\
         3. For the worst failure, if it is tied to an event, call `get_event` on that event id to inspect the underlying call, then summarize the most common failure theme."
    );
    ("Triage recent judge scores and drill into failures", text)
}

fn recent_activity(project: Option<&str>, limit: Option<&str>) -> (&'static str, String) {
    let scope = project
        .map(|p| format!(" for project `{p}`"))
        .unwrap_or_default();
    let events = call_with("query_events", project, limit, "limit");
    let costs = match project {
        Some(p) => format!("`get_cost_summary` for `{p}`"),
        None => "`get_cost_summary`".to_string(),
    };
    let text = format!(
        "Summarize recent LLM activity{scope}.\n\n\
         1. Call {events} and present the events table.\n\
         2. Note any errored calls (❌), the busiest model, and the rough total cost.\n\
         3. Call {costs} for the spend rollup and include the headline total."
    );
    ("Recent LLM calls + spend summary", text)
}

fn margin_report(by: Option<&str>, project: Option<&str>) -> (&'static str, String) {
    let dim = by.unwrap_or("customer");
    let scope = project
        .map(|p| format!(" for project `{p}`"))
        .unwrap_or_default();
    let call = call_with("get_margin", project, Some(dim), "by");
    let text = format!(
        "Report profit margin by {dim}{scope}.\n\n\
         1. Call {call} and present the returned table verbatim — it nets revenue against LLM cost, most-unprofitable first.\n\
         2. Call out every {dim} with a negative margin (🔴) — they cost more in LLM spend than they pay — and any thin-margin ones (⚠️).\n\
         3. For a deeply-negative {dim}, suggest one concrete lever: raise price, cap usage with a limit rule, or move to a cheaper model/prompt."
    );
    ("Profit margin report with money-loser call-outs", text)
}

fn price_book() -> (&'static str, String) {
    (
        "Model price book overview",
        "Show the LightTrack model price book.\n\n\
         Call `list_prices` and present the table verbatim. Then note, per provider, the cheapest and most expensive model by input rate, and flag any model missing a cached-input rate."
            .to_string(),
    )
}

/// Build a `tool (with project `X`, limit N)` call hint from optional args.
fn call_with(tool: &str, project: Option<&str>, limit: Option<&str>, limit_label: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(p) = project {
        parts.push(format!("project `{p}`"));
    }
    if let Some(l) = limit {
        parts.push(format!("{limit_label} {l}"));
    }
    if parts.is_empty() {
        format!("`{tool}`")
    } else {
        format!("`{tool}` (with {})", parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_exposes_all_prompts() {
        let v = list();
        let names: Vec<&str> = v["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"cost-report"));
        assert!(names.contains(&"benchmark-leaderboard"));
        assert!(names.contains(&"margin-report"));
        assert_eq!(names.len(), 7);
    }

    #[test]
    fn get_weaves_in_arguments() {
        let p =
            get(&json!({ "name": "cost-report", "arguments": { "project": "qa-demo" } })).unwrap();
        let text = p["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("project `qa-demo`"));
        assert!(text.contains("get_cost_summary"));
    }

    #[test]
    fn get_degrades_without_required_arg() {
        let p = get(&json!({ "name": "limit-check", "arguments": {} })).unwrap();
        let text = p["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("list_projects")); // graceful fallback, not an error
    }

    #[test]
    fn unknown_prompt_errors() {
        assert!(get(&json!({ "name": "nope" })).is_err());
    }
}
