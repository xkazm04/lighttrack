//! The revenue side of the margin subtraction, and the standing policies that turn an eroding
//! customer into a limit rule without anyone watching.
//!
//! LightTrack observes cost; revenue it has to be told. `revenue record` is the manual telling, and
//! a margin policy is the standing instruction that acts on the difference.

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::cli::{Cli, MarginPoliciesCmd, RevenueCmd};
use crate::http::call;

pub(crate) fn run(cli: &Cli, action: &RevenueCmd) -> Result<()> {
    match action {
        RevenueCmd::Record {
            project,
            amount,
            customer,
            product,
            kind,
            currency,
            external_id,
            source,
            ts,
        } => {
            let mut body = json!({
                "project_id": project,
                "amount_usd": amount,
                "kind": kind,
                "currency": currency,
                "source": source,
            });
            for (k, v) in [
                ("customer_id", customer),
                ("product_id", product),
                ("external_id", external_id),
                ("ts", ts),
            ] {
                if let Some(v) = v {
                    body[k] = json!(v);
                }
            }
            call(cli, Method::POST, "/v1/revenue", Some(body), "")
        }
    }
}

pub(crate) fn run_policies(cli: &Cli, action: &MarginPoliciesCmd) -> Result<()> {
    match action {
        MarginPoliciesCmd::Create {
            project,
            trigger,
            action,
            min_cost_usd,
            cooldown_secs,
            expiry_secs,
            disabled,
        } => {
            let body = policy_body(
                trigger,
                action,
                *min_cost_usd,
                *cooldown_secs,
                *expiry_secs,
                *disabled,
            )?;
            call(
                cli,
                Method::POST,
                &format!("/v1/projects/{project}/margin-policies"),
                Some(body),
                "",
            )
        }
        MarginPoliciesCmd::List { project } => call(
            cli,
            Method::GET,
            &format!("/v1/projects/{project}/margin-policies"),
            None,
            "list_margin_policies",
        ),
        MarginPoliciesCmd::Delete { project, id } => call(
            cli,
            Method::DELETE,
            &format!("/v1/projects/{project}/margin-policies/{id}"),
            None,
            "",
        ),
    }
}

/// `trigger` and `action` are objects the operator types as JSON, so a malformed one is refused
/// here — sending the text through as a string would store a policy that can never arm.
fn policy_body(
    trigger: &str,
    action: &str,
    min_cost_usd: Option<f64>,
    cooldown_secs: Option<i64>,
    expiry_secs: Option<i64>,
    disabled: bool,
) -> Result<Value> {
    let mut body = Map::new();
    body.insert("trigger".into(), object(trigger, "--trigger")?);
    body.insert("action".into(), object(action, "--action")?);
    body.insert("enabled".into(), json!(!disabled));
    if let Some(v) = min_cost_usd {
        body.insert("min_cost_usd".into(), json!(v));
    }
    if let Some(v) = cooldown_secs {
        body.insert("cooldown_secs".into(), json!(v));
    }
    if let Some(v) = expiry_secs {
        body.insert("expiry_secs".into(), json!(v));
    }
    Ok(Value::Object(body))
}

fn object(text: &str, flag: &str) -> Result<Value> {
    let v: Value =
        serde_json::from_str(text).with_context(|| format!("{flag}: invalid JSON: {text}"))?;
    match v {
        Value::Object(_) => Ok(v),
        _ => anyhow::bail!("{flag} must be a JSON object, got: {text}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_policy_that_is_not_json_is_refused_before_the_request() {
        assert!(policy_body("{bad", "{}", None, None, None, false).is_err());
        assert!(policy_body("\"armed\"", "{}", None, None, None, false).is_err());
    }

    /// The optional tunings are absent rather than null, so the API's documented defaults apply;
    /// `--disabled` is the inverse of the wire's `enabled`.
    #[test]
    fn an_untuned_policy_sends_only_what_was_typed() {
        let b = policy_body(
            r#"{"margin_pct_below":0}"#,
            r#"{"metric":"cost_usd"}"#,
            None,
            None,
            None,
            true,
        )
        .expect("body");
        assert_eq!(b["trigger"]["margin_pct_below"], json!(0));
        assert_eq!(b["enabled"], json!(false));
        assert!(b.get("cooldown_secs").is_none(), "{b}");
        assert!(b.get("min_cost_usd").is_none(), "{b}");
    }
}
