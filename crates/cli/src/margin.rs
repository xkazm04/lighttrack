//! The profit reports: margin over a window, its per-day trend, one customer's split, and the
//! pricing what-if. The revenue side of the subtraction lives in `revenue`.

use anyhow::Result;
use reqwest::Method;

use crate::cli::{Cli, MarginArgs, MarginCmd};
use crate::http::call;
use crate::query::{encode, Query};

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

pub(crate) fn trend_path(
    by: &str,
    project: &Option<String>,
    days: Option<i64>,
    top: Option<i64>,
) -> String {
    let mut q = Query::new("/v1/margin/trend");
    q.push("by", Some(by));
    q.push("project", project.as_deref());
    q.push_raw("days", days);
    q.push_raw("top", top);
    q.finish()
}

pub(crate) fn customer_path(
    id: &str,
    project: &Option<String>,
    since: &Option<String>,
    until: &Option<String>,
) -> String {
    let mut q = Query::new(&format!("/v1/margin/customer/{}", encode(id)));
    q.push("project", project.as_deref());
    q.push("since", since.as_deref());
    q.push("until", until.as_deref());
    q.finish()
}

pub(crate) fn simulate_path(
    by: &str,
    project: &Option<String>,
    price_per_mtok: Option<f64>,
    flat_monthly: Option<f64>,
    since: &Option<String>,
    until: &Option<String>,
) -> String {
    let mut q = Query::new("/v1/margin/simulate");
    q.push("by", Some(by));
    q.push("project", project.as_deref());
    q.push_raw("price_per_mtok", price_per_mtok);
    q.push_raw("flat_monthly", flat_monthly);
    q.push("since", since.as_deref());
    q.push("until", until.as_deref());
    q.finish()
}

pub(crate) fn run(cli: &Cli, args: &MarginArgs, action: &Option<MarginCmd>) -> Result<()> {
    match action {
        None => call(
            cli,
            Method::GET,
            &margin_path(&args.by, &args.project, &args.since, &args.until),
            None,
            "get_margin",
        ),
        Some(MarginCmd::Trend {
            by,
            project,
            days,
            top,
        }) => call(
            cli,
            Method::GET,
            &trend_path(by, project, *days, *top),
            None,
            "get_margin_trend",
        ),
        Some(MarginCmd::Customer {
            id,
            project,
            since,
            until,
        }) => call(
            cli,
            Method::GET,
            &customer_path(id, project, since, until),
            None,
            "get_margin_customer",
        ),
        Some(MarginCmd::Simulate {
            by,
            project,
            price_per_mtok,
            flat_monthly,
            since,
            until,
        }) => call(
            cli,
            Method::GET,
            &simulate_path(by, project, *price_per_mtok, *flat_monthly, since, until),
            None,
            "get_margin_simulate",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
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

    /// An omitted `--days`/`--top` must not be sent: the API's own defaults (30 / 20) are the
    /// documented ones, and `days=` would be a 400 rather than a default.
    #[test]
    fn trend_sends_only_the_window_it_was_given() {
        assert_eq!(
            trend_path("customer", &None, None, None),
            "/v1/margin/trend?by=customer"
        );
        assert_eq!(
            trend_path("product", &s("p1"), Some(7), None),
            "/v1/margin/trend?by=product&project=p1&days=7"
        );
    }

    /// A customer id is operator data and routinely carries `/` or `@`; unencoded it would change
    /// the route rather than the lookup.
    #[test]
    fn a_customer_id_is_encoded_into_the_path() {
        assert_eq!(
            customer_path("acme/eu", &None, &None, &None),
            "/v1/margin/customer/acme%2Feu"
        );
        assert_eq!(
            customer_path("acme", &s("p1"), &None, &None),
            "/v1/margin/customer/acme?project=p1"
        );
    }

    #[test]
    fn simulate_carries_whichever_price_model_was_named() {
        assert_eq!(
            simulate_path("customer", &None, Some(12.5), None, &None, &None),
            "/v1/margin/simulate?by=customer&price_per_mtok=12.5"
        );
        assert_eq!(
            simulate_path("customer", &None, None, Some(99.0), &None, &None),
            "/v1/margin/simulate?by=customer&flat_monthly=99"
        );
    }
}
