//! `lt prices` — the model price book, the traffic it could not cost, and the rate timeline.

use anyhow::Result;
use reqwest::Method;

use crate::cli::{Cli, PricesCmd};
use crate::http::call;

/// `/v1/costs/unpriced` with the optional narrowing. Both parameters are optional: "what are we
/// failing to price" has a useful answer before you know which project or window to ask about.
pub(crate) fn unpriced_path(project: &Option<String>, since: &Option<String>) -> String {
    let mut p = "/v1/costs/unpriced".to_string();
    let mut sep = '?';
    for (k, v) in [("project", project), ("since", since)] {
        if let Some(val) = v.as_deref().filter(|s| !s.is_empty()) {
            p.push_str(&format!("{sep}{k}={val}"));
            sep = '&';
        }
    }
    p
}

pub(crate) fn run(cli: &Cli, action: &PricesCmd) -> Result<()> {
    match action {
        PricesCmd::List => call(cli, Method::GET, "/v1/prices", None, "list_prices"),
        PricesCmd::Unpriced { project, since } => call(
            cli,
            Method::GET,
            &unpriced_path(project, since),
            None,
            "list_unpriced_models",
        ),
        PricesCmd::History { provider, model } => call(
            cli,
            Method::GET,
            &format!("/v1/prices/history/{provider}/{model}"),
            None,
            "list_price_history",
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
    fn the_unpriced_path_narrows_only_when_asked() {
        assert_eq!(unpriced_path(&None, &None), "/v1/costs/unpriced");
        assert_eq!(
            unpriced_path(&s("p1"), &None),
            "/v1/costs/unpriced?project=p1"
        );
        assert_eq!(
            unpriced_path(&s("p1"), &s("2026-01-01T00:00:00Z")),
            "/v1/costs/unpriced?project=p1&since=2026-01-01T00:00:00Z"
        );
        // `since` alone must open the query string, not join an absent `project` with `&`.
        assert_eq!(
            unpriced_path(&None, &s("2026-01-01T00:00:00Z")),
            "/v1/costs/unpriced?since=2026-01-01T00:00:00Z"
        );
    }
}
