//! Limit rules: create, replace, delete, list, and the live status view.

use anyhow::Result;
use reqwest::Method;
use serde_json::{json, Value};

use crate::cli::{Cli, LimitsCmd};
use crate::http::call;

/// Build the optional dimension-scope object for a limit rule from the CLI's mutually-exclusive
/// `--scope-*` flags (clap enforces at most one). `null` (unscoped) when none is set.
pub(crate) fn scope_json(
    provider: &Option<String>,
    model: &Option<String>,
    name: &Option<String>,
) -> Value {
    if let Some(v) = provider {
        json!({ "provider": v })
    } else if let Some(v) = model {
        json!({ "model": v })
    } else if let Some(v) = name {
        json!({ "name": v })
    } else {
        Value::Null
    }
}

/// The rule body `set` (POST) and `update` (PUT) both send — one shaping, so the create and replace
/// paths cannot drift into writing different fields. `--disabled` is the inverse of the wire's
/// `enabled`.
fn rule_body(
    metric: &str,
    window: &str,
    threshold: f64,
    action: &str,
    disabled: bool,
    warn_at: &Option<f64>,
    scope: Value,
) -> Value {
    json!({
        "metric": metric, "window": window,
        "threshold": threshold, "action": action, "enabled": !disabled,
        "warn_at": warn_at,
        "scope": scope
    })
}

pub(crate) fn run(cli: &Cli, action: &LimitsCmd) -> Result<()> {
    match action {
        LimitsCmd::Set {
            project,
            metric,
            window,
            threshold,
            action,
            disabled,
            warn_at,
            scope_provider,
            scope_model,
            scope_name,
        } => call(
            cli,
            Method::POST,
            &format!("/v1/projects/{project}/limits"),
            Some(rule_body(
                metric,
                window,
                *threshold,
                action,
                *disabled,
                warn_at,
                scope_json(scope_provider, scope_model, scope_name),
            )),
            "",
        ),
        LimitsCmd::Update {
            id,
            metric,
            window,
            threshold,
            action,
            disabled,
            warn_at,
            scope_provider,
            scope_model,
            scope_name,
        } => call(
            cli,
            Method::PUT,
            &format!("/v1/limits/{id}"),
            Some(rule_body(
                metric,
                window,
                *threshold,
                action,
                *disabled,
                warn_at,
                scope_json(scope_provider, scope_model, scope_name),
            )),
            "",
        ),
        LimitsCmd::Delete { id } => call(cli, Method::DELETE, &format!("/v1/limits/{id}"), None, ""),
        LimitsCmd::List { project } => call(
            cli,
            Method::GET,
            &format!("/v1/projects/{project}/limits"),
            None,
            "list_limits",
        ),
        LimitsCmd::Status { project } => call(
            cli,
            Method::GET,
            &format!("/v1/limits/status?project={project}"),
            None,
            "get_limit_status",
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
    fn scope_json_picks_the_one_dimension_that_is_set() {
        assert_eq!(scope_json(&s("openai"), &None, &None), json!({ "provider": "openai" }));
        assert_eq!(scope_json(&None, &s("gpt-4o"), &None), json!({ "model": "gpt-4o" }));
        assert_eq!(scope_json(&None, &None, &s("summarize")), json!({ "name": "summarize" }));
    }

    /// An unscoped rule must send `null`, not `{}` — the API reads a missing scope as "whole
    /// project", and an empty object would not deserialize into one.
    #[test]
    fn unscoped_rule_sends_null() {
        assert_eq!(scope_json(&None, &None, &None), Value::Null);
    }

    /// Precedence is only reachable if clap's `group` is ever loosened; pin it so the behavior is
    /// deliberate rather than incidental.
    #[test]
    fn provider_wins_over_model_and_name() {
        assert_eq!(
            scope_json(&s("openai"), &s("gpt-4o"), &s("summarize")),
            json!({ "provider": "openai" })
        );
    }

    #[test]
    fn rule_body_inverts_disabled_into_enabled() {
        let on = rule_body("cost_usd", "day", 5.0, "alert", false, &None, Value::Null);
        assert_eq!(on["enabled"], json!(true));
        let off = rule_body("cost_usd", "day", 5.0, "alert", true, &None, Value::Null);
        assert_eq!(off["enabled"], json!(false));
    }

    #[test]
    fn rule_body_carries_every_field_the_api_expects() {
        let b = rule_body(
            "tokens",
            "hour",
            1000.0,
            "block",
            false,
            &Some(0.8),
            json!({ "model": "gpt-4o" }),
        );
        assert_eq!(b["metric"], json!("tokens"));
        assert_eq!(b["window"], json!("hour"));
        assert_eq!(b["threshold"], json!(1000.0));
        assert_eq!(b["action"], json!("block"));
        assert_eq!(b["warn_at"], json!(0.8));
        assert_eq!(b["scope"], json!({ "model": "gpt-4o" }));
    }

    /// An omitted `--warn-at` is an explicit `null`, which is how the API distinguishes "no soft
    /// warning" from a value it should keep.
    #[test]
    fn absent_warn_at_is_null_not_missing() {
        let b = rule_body("cost_usd", "day", 5.0, "alert", false, &None, Value::Null);
        assert_eq!(b.get("warn_at"), Some(&Value::Null));
    }
}
