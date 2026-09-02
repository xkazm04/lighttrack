//! `margin_policies` collection: create / list / get / delete.
//!
//! Same shape as the SQLite and Postgres backends — `trigger` and `action` ride as JSON strings
//! because both are open sum types. Declaring this surface rather than inheriting the trait's
//! `Unsupported` matters: a Firestore deployment answering `[]` would tell an operator their
//! guardrails simply never fired, instead of that the table was never ported.

use serde_json::json;

use lighttrack_core::{MarginPolicy, PolicyAction, PolicyTrigger};
use lighttrack_store::{Result, StoreError};

use crate::codec::*;
use crate::rest::Rest;

const COLL: &str = "margin_policies";

fn policy_fields(p: &MarginPolicy) -> Result<Fields> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(p.id));
    m.insert("project_id".into(), json!(p.project_id));
    m.insert(
        "trigger_json".into(),
        json!(serde_json::to_string(&p.trigger)?),
    );
    m.insert("min_cost_usd".into(), json!(p.min_cost_usd));
    m.insert(
        "action_json".into(),
        json!(serde_json::to_string(&p.action)?),
    );
    m.insert("cooldown_secs".into(), json!(p.cooldown_secs as i64));
    m.insert("expiry_secs".into(), json!(p.expiry_secs as i64));
    m.insert("enabled".into(), json!(p.enabled as i64));
    Ok(m)
}

pub(crate) fn create_margin_policy(rest: &Rest, p: &MarginPolicy) -> Result<()> {
    rest.put_doc(COLL, &p.id, &policy_fields(p)?)
}

pub(crate) fn list_margin_policies(
    rest: &Rest,
    project: &str,
    only_enabled: bool,
) -> Result<Vec<MarginPolicy>> {
    let mut filters: Vec<(&str, &str, serde_json::Value)> =
        vec![("project_id", "EQUAL", json!(project))];
    if only_enabled {
        filters.push(("enabled", "EQUAL", json!(1_i64)));
    }
    let docs = rest.query(COLL, &filters, None, None)?;
    docs.iter().map(policy_from).collect()
}

pub(crate) fn get_margin_policy(rest: &Rest, id: &str) -> Result<Option<MarginPolicy>> {
    rest.get_doc(COLL, id)?
        .as_ref()
        .map(policy_from)
        .transpose()
}

pub(crate) fn delete_margin_policy(rest: &Rest, id: &str) -> Result<bool> {
    rest.delete_doc(COLL, id)
}

fn policy_from(m: &Fields) -> Result<MarginPolicy> {
    let id: String = freq(m, "id")?;
    let trigger: PolicyTrigger = serde_json::from_str(&fstr(m, "trigger_json").unwrap_or_default())
        .map_err(|e| {
            StoreError::Other(format!(
                "margin policy '{id}' has an unreadable trigger: {e}"
            ))
        })?;
    let action: PolicyAction = serde_json::from_str(&fstr(m, "action_json").unwrap_or_default())
        .map_err(|e| {
            StoreError::Other(format!(
                "margin policy '{id}' has an unreadable action: {e}"
            ))
        })?;
    Ok(MarginPolicy {
        id,
        project_id: freq(m, "project_id")?,
        trigger,
        min_cost_usd: ff64(m, "min_cost_usd").unwrap_or(0.0),
        action,
        cooldown_secs: fi64(m, "cooldown_secs").unwrap_or(3600).max(0) as u64,
        expiry_secs: fi64(m, "expiry_secs").unwrap_or(86_400).max(0) as u64,
        enabled: fbool(m, "enabled"),
    })
}
