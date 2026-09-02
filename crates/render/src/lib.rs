//! lighttrack-render — turns LightTrack API JSON into compact, human-readable Markdown.
//!
//! Both `lt-mcp` and the `lt` CLI feed raw `serde_json::Value` responses through [`render`], which
//! returns a Markdown view (aligned tables, status glyphs, sparklines) for the human, or `None` when
//! no renderer matches the `kind` — callers then fall back to pretty JSON. Pure string work: no I/O,
//! and deliberately no `core` dependency, so it stays a thin Value-in / Markdown-out layer that mirrors
//! how the MCP server and CLI already pass untyped JSON around.

use serde_json::Value;

mod alerts;
mod benchmarks;
mod collective;
mod compare;
mod costs;
mod datasets;
mod events;
mod forecast;
mod jobs;
mod labels;
mod limits;
mod margin;
mod md;
mod prices;
mod projects;
mod prompts;
mod rollup;
mod rubrics;
mod schedules;
mod scores;
mod traces;
mod unpriced;

/// Render an API response to Markdown for the given logical `kind` (an MCP tool name, or the matching
/// CLI verb). Returns `None` when there is no renderer for `kind`, or the value shape is unexpected —
/// the caller is expected to fall back to raw pretty JSON in that case.
pub fn render(kind: &str, v: &Value) -> Option<String> {
    match kind {
        "list_projects" => projects::list(v),
        "get_cost_summary" => costs::summary(v),
        "get_usecases" => costs::usecases(v),
        "query_rollup" => rollup::table(v),
        "get_forecast" => forecast::report(v),
        "query_events" => events::list(v),
        "get_event" => events::detail(v),
        "list_traces" => traces::list(v),
        "get_trace" => traces::tree(v),
        "list_scores" => scores::list(v),
        "get_limit_status" => limits::status(v),
        "list_limits" => limits::list(v),
        "list_alerts" => alerts::list(v),
        "list_prices" => prices::list(v),
        "list_price_history" => prices::history(v),
        "list_unpriced_models" => unpriced::ledger(v),
        "list_benchmarks" => benchmarks::list(v),
        "get_benchmark" => benchmarks::detail(v),
        "get_benchmark_runs" => benchmarks::runs(v),
        "check_benchmark_gate" => benchmarks::gate(v),
        "list_jobs" => jobs::list(v),
        "list_schedules" => schedules::list(v),
        "get_job" => jobs::detail(v),
        "list_datasets" => datasets::list(v),
        "get_dataset" => datasets::detail(v),
        "list_dataset_items" => datasets::items(v),
        "list_labels" => labels::list(v),
        "list_calibrations" => labels::calibrations(v),
        "get_judge_trust" => labels::trust(v),
        "list_rubrics" => rubrics::list(v),
        "get_rubric" => rubrics::detail(v),
        "list_prompts" => prompts::list(v),
        "get_prompt" => prompts::resolved(v),
        "compare" => compare::leaderboard(v),
        "get_margin" => margin::report(v),
        "get_margin_trend" => margin::trend(v),
        "get_margin_customer" => margin::customer(v),
        "get_margin_simulate" => margin::simulate(v),
        "get_collective_leaderboard" => collective::leaderboard(v),
        "get_collective_digest" => collective::digest(v),
        _ => None,
    }
}
