//! lighttrack-render — turns LightTrack API JSON into compact, human-readable Markdown.
//!
//! Both `lt-mcp` and the `lt` CLI feed raw `serde_json::Value` responses through [`render`], which
//! returns a Markdown view (aligned tables, status glyphs, sparklines) for the human, or `None` when
//! no renderer matches the `kind` — callers then fall back to pretty JSON. Pure string work: no I/O,
//! and deliberately no `core` dependency, so it stays a thin Value-in / Markdown-out layer that mirrors
//! how the MCP server and CLI already pass untyped JSON around.
//!
//! The dispatch is a **table**, not a `match`, so it can be enumerated: the tests below hold it to
//! `lighttrack-contract` in both directions — every `render_kind` an endpoint declares has a
//! renderer here, and every renderer here has something that produces its input. A renderer with no
//! producer is dead code that reads as a feature, which is what several of these arms had become.

use serde_json::Value;

mod alerts;
mod benchmarks;
mod capabilities;
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
mod margin_policies;
mod md;
mod prices;
mod projects;
mod prompts;
mod quality;
mod rollup;
mod rubrics;
mod schedules;
mod scores;
mod traces;
mod unpriced;

/// One renderer: the logical kind it answers to, and the function that renders it.
type Renderer = (&'static str, fn(&Value) -> Option<String>);

/// The dispatch table. Keys are `Endpoint.render_kind` values from `lighttrack-contract` (which are
/// the MCP tool names, and the matching CLI verbs), plus the small declared set in
/// [`NON_ENDPOINT_KINDS`].
const RENDERERS: &[Renderer] = &[
    ("get_capabilities", capabilities::manifest),
    ("list_projects", projects::list),
    ("get_cost_summary", costs::summary),
    ("get_usecases", costs::usecases),
    ("query_rollup", rollup::table),
    ("get_forecast", forecast::report),
    ("query_events", events::list),
    ("get_event", events::detail),
    ("list_traces", traces::list),
    ("get_trace", traces::tree),
    ("list_scores", scores::list),
    ("get_limit_status", limits::status),
    ("list_limits", limits::list),
    ("list_alerts", alerts::list),
    ("list_prices", prices::list),
    ("list_price_history", prices::history),
    ("list_unpriced_models", unpriced::ledger),
    ("list_benchmarks", benchmarks::list),
    ("get_benchmark", benchmarks::detail),
    ("get_benchmark_runs", benchmarks::runs),
    ("check_benchmark_gate", benchmarks::gate),
    ("list_jobs", jobs::list),
    ("list_schedules", schedules::list),
    ("get_job", jobs::detail),
    ("list_datasets", datasets::list),
    ("get_dataset", datasets::detail),
    ("list_dataset_items", datasets::items),
    ("list_labels", labels::list),
    ("list_calibrations", labels::calibrations),
    ("get_judge_trust", labels::trust),
    ("list_rubrics", rubrics::list),
    ("get_rubric", rubrics::detail),
    ("list_prompts", prompts::list),
    ("get_prompt", prompts::resolved),
    ("get_prompt_quality", quality::table),
    ("compare", compare::leaderboard),
    ("get_margin", margin::report),
    ("list_margin_policies", margin_policies::list),
    ("get_margin_trend", margin::trend),
    ("get_margin_customer", margin::customer),
    ("get_margin_simulate", margin::simulate),
    ("get_collective_leaderboard", collective::leaderboard),
    ("get_collective_digest", collective::digest),
    ("get_collective_contributions", collective::contributions),
];

#[cfg(test)]
/// Kinds that legitimately have no HTTP endpoint behind them. `compare` is `lt-runner`'s own
/// compare-benchmark summary, assembled in the runner and never fetched from the API — so it is a
/// renderer with a producer, just not a route. Anything else missing from the contract is dead.
const NON_ENDPOINT_KINDS: &[&str] = &["compare"];

/// Render an API response to Markdown for the given logical `kind` (an MCP tool name, or the matching
/// CLI verb). Returns `None` when there is no renderer for `kind`, or the value shape is unexpected —
/// the caller is expected to fall back to raw pretty JSON in that case.
pub fn render(kind: &str, v: &Value) -> Option<String> {
    let (_, f) = RENDERERS.iter().find(|(k, _)| *k == kind)?;
    f(v)
}

/// Every kind this crate can render. Enumerable so the contract tests can be two-directional.
pub fn kinds() -> impl Iterator<Item = &'static str> {
    RENDERERS.iter().map(|(k, _)| *k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_kind_is_registered_twice() {
        let mut ks: Vec<&str> = kinds().collect();
        let before = ks.len();
        ks.sort_unstable();
        ks.dedup();
        assert_eq!(before, ks.len(), "a duplicate kind shadows a renderer");
    }

    /// Half of the bijection: an endpoint that declares a `render_kind` nothing renders would show
    /// an agent raw JSON while claiming a table — a silent downgrade, exactly the drift this table
    /// exists to make impossible.
    #[test]
    fn every_render_kind_the_contract_declares_has_a_renderer() {
        let missing: Vec<&str> = lighttrack_contract::endpoints()
            .filter_map(|e| e.render_kind)
            .filter(|k| !RENDERERS.iter().any(|(r, _)| r == k))
            .collect();
        assert!(
            missing.is_empty(),
            "the contract declares these render kinds, and nothing here renders them: {missing:?}"
        );
    }

    /// The other half: a renderer nobody can reach is dead code that reads as a feature. Several
    /// arms here were exactly that before the contract could say so.
    #[test]
    fn every_renderer_has_something_that_produces_its_input() {
        let dead: Vec<&str> = kinds()
            .filter(|k| !NON_ENDPOINT_KINDS.contains(k))
            .filter(|k| !lighttrack_contract::endpoints().any(|e| e.render_kind == Some(*k)))
            .collect();
        assert!(
            dead.is_empty(),
            "these renderers have no producing endpoint: {dead:?}. Either give the endpoint a \
             render_kind, or delete the renderer — do not leave it looking supported."
        );
    }

    /// An empty result set is the common case on a fresh deployment. A renderer that panics or
    /// returns a header with no body there is the first thing a new user sees.
    #[test]
    fn every_renderer_survives_an_empty_response() {
        for (kind, f) in RENDERERS {
            for empty in [json!([]), json!({}), json!({ "items": [] }), Value::Null] {
                // The contract is only that it does not panic and does not lie: `None` (fall back
                // to JSON) is a perfectly good answer for a shape a renderer does not recognise.
                let out = f(&empty);
                if let Some(md) = out {
                    assert!(
                        !md.is_empty(),
                        "{kind} rendered an empty string for {empty}; return None instead so the \
                         caller falls back to JSON"
                    );
                }
            }
        }
    }

    #[test]
    fn an_unknown_kind_falls_back_rather_than_guessing() {
        assert!(render("no_such_tool", &json!({})).is_none());
    }
}
