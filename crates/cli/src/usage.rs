//! Read-only usage views: cost rollups, events, traces, and the profit-margin report.

use anyhow::Result;
use reqwest::Method;

use crate::cli::Cli;
use crate::http::call;

/// `base`, plus `?project=` when the caller scoped the read. Used by the endpoints that take no
/// other query parameter.
pub(crate) fn path_with_project(base: &str, project: &Option<String>) -> String {
    match project {
        Some(p) => format!("{base}?project={p}"),
        None => base.to_string(),
    }
}

/// A paged listing path (`/v1/events`, `/v1/traces`): `limit` always, `project` when scoped.
pub(crate) fn listing_path(base: &str, project: &Option<String>, limit: usize) -> String {
    let mut p = format!("{base}?limit={limit}");
    if let Some(proj) = project {
        p.push_str(&format!("&project={proj}"));
    }
    p
}

/// The margin report path. `by` is always sent (clap defaults it); the window bounds and project
/// are appended only when given, so the API applies its own defaults otherwise.
pub(crate) fn margin_path(
    by: &str,
    project: &Option<String>,
    since: &Option<String>,
    until: &Option<String>,
) -> String {
    let mut p = format!("/v1/margin?by={by}");
    for (k, v) in [("project", project), ("since", since), ("until", until)] {
        if let Some(val) = v {
            p.push_str(&format!("&{k}={val}"));
        }
    }
    p
}

pub(crate) fn costs(cli: &Cli, project: &Option<String>) -> Result<()> {
    call(
        cli,
        Method::GET,
        &path_with_project("/v1/costs", project),
        None,
        "get_cost_summary",
    )
}

pub(crate) fn events(cli: &Cli, project: &Option<String>, limit: usize) -> Result<()> {
    call(
        cli,
        Method::GET,
        &listing_path("/v1/events", project, limit),
        None,
        "query_events",
    )
}

pub(crate) fn traces(cli: &Cli, project: &Option<String>, limit: usize) -> Result<()> {
    call(
        cli,
        Method::GET,
        &listing_path("/v1/traces", project, limit),
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

pub(crate) fn margin(
    cli: &Cli,
    by: &str,
    project: &Option<String>,
    since: &Option<String>,
    until: &Option<String>,
) -> Result<()> {
    call(
        cli,
        Method::GET,
        &margin_path(by, project, since, until),
        None,
        "get_margin",
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
        assert_eq!(listing_path("/v1/events", &None, 20), "/v1/events?limit=20");
        assert_eq!(
            listing_path("/v1/traces", &s("p1"), 5),
            "/v1/traces?limit=5&project=p1"
        );
    }

    #[test]
    fn margin_path_appends_only_the_bounds_that_were_given() {
        assert_eq!(
            margin_path("customer", &None, &None, &None),
            "/v1/margin?by=customer"
        );
        assert_eq!(
            margin_path("product", &s("p1"), &s("2026-01-01T00:00:00Z"), &None),
            "/v1/margin?by=product&project=p1&since=2026-01-01T00:00:00Z"
        );
        assert_eq!(
            margin_path("customer", &None, &None, &s("2026-02-01T00:00:00Z")),
            "/v1/margin?by=customer&until=2026-02-01T00:00:00Z"
        );
    }
}
