//! Read-only usage views: cost rollups, the grouping primitive under them, events, traces, the
//! forecast, and the FX restatement.

use anyhow::Result;
use reqwest::Method;

use crate::cli::{Cli, CostsCmd};
use crate::http::call;
use crate::query::Query;

/// `base`, plus `?project=` when the caller scoped the read. Used by the endpoints that take no
/// other query parameter.
pub(crate) fn path_with_project(base: &str, project: &Option<String>) -> String {
    match project {
        Some(p) => format!("{base}?project={p}"),
        None => base.to_string(),
    }
}

/// A paged listing path (`/v1/events`, `/v1/traces`): `limit` always, then the narrowings that were
/// given. Without `--cursor` these endpoints can only ever answer with their first page.
pub(crate) fn listing_path(
    base: &str,
    project: &Option<String>,
    limit: usize,
    cursor: &Option<String>,
) -> String {
    let mut q = Query::new(base);
    q.push_raw("limit", Some(limit));
    q.push("project", project.as_deref());
    q.push("cursor", cursor.as_deref());
    q.finish()
}

/// `/v1/costs/prompts` — the same window as `lt costs`, grouped by the served version tag.
pub(crate) fn prompt_costs_path(
    project: &Option<String>,
    since: &Option<String>,
    until: &Option<String>,
) -> String {
    let mut q = Query::new("/v1/costs/prompts");
    q.push("project", project.as_deref());
    q.push("since", since.as_deref());
    q.push("until", until.as_deref());
    q.finish()
}

/// `/v1/rollup`. `by` is always sent (clap defaults it); everything else is appended only when
/// given, so the API applies its own defaults rather than being told a guess.
pub(crate) fn rollup_path(
    project: &Option<String>,
    by: &str,
    since: &Option<String>,
    until: &Option<String>,
    time: &Option<String>,
    filter: &Option<String>,
) -> String {
    let mut q = Query::new("/v1/rollup");
    q.push("by", Some(by));
    q.push("project", project.as_deref());
    q.push("since", since.as_deref());
    q.push("until", until.as_deref());
    q.push("time", time.as_deref());
    q.push("filter", filter.as_deref());
    q.finish()
}

pub(crate) fn forecast_path(
    project: &Option<String>,
    by: &str,
    horizon: Option<i64>,
    lookback: Option<i64>,
) -> String {
    let mut q = Query::new("/v1/forecast");
    q.push("by", Some(by));
    q.push("project", project.as_deref());
    q.push_raw("horizon", horizon);
    q.push_raw("lookback", lookback);
    q.finish()
}

pub(crate) fn costs(cli: &Cli, project: &Option<String>, action: &Option<CostsCmd>) -> Result<()> {
    match action {
        Some(CostsCmd::Prompts {
            project,
            since,
            until,
        }) => call(
            cli,
            Method::GET,
            &prompt_costs_path(project, since, until),
            None,
            "",
        ),
        None => call(
            cli,
            Method::GET,
            &path_with_project("/v1/costs", project),
            None,
            "get_cost_summary",
        ),
    }
}

pub(crate) fn rollup(
    cli: &Cli,
    project: &Option<String>,
    by: &str,
    since: &Option<String>,
    until: &Option<String>,
    time: &Option<String>,
    filter: &Option<String>,
) -> Result<()> {
    call(
        cli,
        Method::GET,
        &rollup_path(project, by, since, until, time, filter),
        None,
        "query_rollup",
    )
}

pub(crate) fn forecast(
    cli: &Cli,
    project: &Option<String>,
    by: &str,
    horizon: Option<i64>,
    lookback: Option<i64>,
) -> Result<()> {
    call(
        cli,
        Method::GET,
        &forecast_path(project, by, horizon, lookback),
        None,
        "get_forecast",
    )
}

pub(crate) fn events(
    cli: &Cli,
    project: &Option<String>,
    limit: usize,
    cursor: &Option<String>,
) -> Result<()> {
    call(
        cli,
        Method::GET,
        &listing_path("/v1/events", project, limit, cursor),
        None,
        "query_events",
    )
}

pub(crate) fn traces(
    cli: &Cli,
    project: &Option<String>,
    limit: usize,
    cursor: &Option<String>,
) -> Result<()> {
    call(
        cli,
        Method::GET,
        &listing_path("/v1/traces", project, limit, cursor),
        None,
        "list_traces",
    )
}

pub(crate) fn trace(cli: &Cli, id: &str) -> Result<()> {
    call(
        cli,
        Method::GET,
        &format!("/v1/traces/{id}"),
        None,
        "get_trace",
    )
}

/// The query for `lt reprice`. `dry_run` is inverted from `--apply` on purpose: the destructive
/// form is the one you have to type, matching the route's own default.
pub(crate) fn reprice_path(
    currency: &str,
    project: &Option<String>,
    rate: &Option<f64>,
    apply: bool,
) -> String {
    let mut p = format!("/v1/revenue/reprice?currency={currency}&dry_run={}", !apply);
    if let Some(proj) = project {
        p.push_str(&format!("&project={proj}"));
    }
    if let Some(r) = rate {
        p.push_str(&format!("&rate={r}"));
    }
    p
}

pub(crate) fn reprice(
    cli: &Cli,
    currency: &str,
    project: &Option<String>,
    rate: &Option<f64>,
    apply: bool,
) -> Result<()> {
    call(
        cli,
        Method::POST,
        &reprice_path(currency, project, rate, apply),
        None,
        "reprice_revenue",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    /// The safe default has to survive the CLI layer too: no `--apply` must reach the server as a
    /// dry run, and `--apply` as a real one.
    #[test]
    fn reprice_previews_unless_apply_is_given() {
        assert_eq!(
            reprice_path("GBP", &None, &None, false),
            "/v1/revenue/reprice?currency=GBP&dry_run=true"
        );
        assert_eq!(
            reprice_path("GBP", &s("p1"), &Some(1.27), true),
            "/v1/revenue/reprice?currency=GBP&dry_run=false&project=p1&rate=1.27"
        );
    }

    #[test]
    fn path_with_project_omits_the_query_when_unscoped() {
        assert_eq!(path_with_project("/v1/costs", &None), "/v1/costs");
        assert_eq!(
            path_with_project("/v1/costs", &s("p1")),
            "/v1/costs?project=p1"
        );
    }

    /// `limit` opens the query string, so `project` must join with `&` — swapping the separators
    /// would send the project as part of the limit value.
    #[test]
    fn listing_path_orders_limit_then_project() {
        assert_eq!(
            listing_path("/v1/events", &None, 20, &None),
            "/v1/events?limit=20"
        );
        assert_eq!(
            listing_path("/v1/traces", &s("p1"), 5, &None),
            "/v1/traces?limit=5&project=p1"
        );
    }

    /// A cursor is opaque and routinely carries `+`, `/` and `=`; pasted raw it would decode to a
    /// different position, which shows up as a silently wrong page rather than an error.
    #[test]
    fn a_cursor_is_sent_only_when_given_and_is_encoded() {
        assert!(!listing_path("/v1/events", &None, 20, &Some(String::new())).contains("cursor"));
        assert_eq!(
            listing_path("/v1/events", &None, 20, &s("aa+bb/cc=")),
            "/v1/events?limit=20&cursor=aa%2Bbb%2Fcc%3D"
        );
    }

    #[test]
    fn the_prompt_cost_window_is_sent_only_where_it_was_given() {
        assert_eq!(prompt_costs_path(&None, &None, &None), "/v1/costs/prompts");
        // A later parameter alone must still open the query string.
        assert_eq!(
            prompt_costs_path(&None, &None, &s("2026-02-01T00:00:00Z")),
            "/v1/costs/prompts?until=2026-02-01T00%3A00%3A00Z"
        );
    }

    /// `by` is clap-defaulted, so it is always present and always first; the optional narrowings
    /// join it rather than opening a second query string.
    #[test]
    fn rollup_always_sends_a_grouping_and_only_the_given_filters() {
        assert_eq!(
            rollup_path(&None, "provider,model", &None, &None, &None, &None),
            "/v1/rollup?by=provider%2Cmodel"
        );
        let p = rollup_path(
            &s("p1"),
            "customer,day",
            &None,
            &None,
            &s("received_at"),
            &s("model:gpt-5.4"),
        );
        assert!(p.starts_with("/v1/rollup?by=customer%2Cday"), "{p}");
        assert!(p.contains("&project=p1"), "{p}");
        assert!(p.contains("&time=received_at"), "{p}");
        assert!(p.contains("&filter=model%3Agpt-5.4"), "{p}");
    }

    #[test]
    fn forecast_omits_the_horizons_it_was_not_given() {
        assert_eq!(
            forecast_path(&None, "customer", None, None),
            "/v1/forecast?by=customer"
        );
        assert_eq!(
            forecast_path(&s("p1"), "product", Some(30), Some(60)),
            "/v1/forecast?by=product&project=p1&horizon=30&lookback=60"
        );
    }
}
