//! `limit_rules` collection: create / list / get / update / delete, with faithful
//! `warn_at` + `scope` round-trip (a backend that drops scope silently widens a scoped cap
//! to the whole project).

use serde_json::{json, Value};

use lighttrack_core::{Escalation, LimitRule, LimitScope, Threshold};
use lighttrack_store::Result;

use crate::codec::*;
use crate::rest::Rest;

const COLL: &str = "limit_rules";

fn limit_fields(r: &LimitRule) -> Result<Fields> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(r.id));
    m.insert("project_id".into(), json!(r.project_id));
    m.insert("metric".into(), json!(enum_to_str(&r.metric)?));
    m.insert("window".into(), json!(enum_to_str(&r.window)?));
    // A plain `Fixed` cap keeps writing the bare number it always did (so a document written before
    // derived thresholds existed reads back byte-identically); anything richer rides in
    // `threshold_json`, whose presence is what `limit_from` keys on.
    let (threshold, threshold_json) = match &r.threshold {
        Threshold::Fixed(v) => (*v, Value::Null),
        other => (0.0, json!(serde_json::to_string(other)?)),
    };
    m.insert("threshold".into(), json!(threshold));
    m.insert("threshold_json".into(), threshold_json);
    m.insert(
        "escalation_json".into(),
        match &r.escalation {
            None => Value::Null,
            Some(e) => json!(serde_json::to_string(e)?),
        },
    );
    m.insert(
        "escalated_until".into(),
        match r.escalated_until {
            None => Value::Null,
            Some(t) => json!(fmt_ts(t)),
        },
    );
    m.insert("origin".into(), json!(r.origin));
    m.insert(
        "expires_at".into(),
        match r.expires_at {
            None => Value::Null,
            Some(t) => json!(fmt_ts(t)),
        },
    );
    m.insert("action".into(), json!(enum_to_str(&r.action)?));
    m.insert("enabled".into(), json!(r.enabled as i64));
    m.insert("warn_at".into(), json!(r.warn_at));
    let (kind, value) = match &r.scope {
        None => (Value::Null, Value::Null),
        Some(s) => (json!(s.kind_str()), json!(s.value())),
    };
    m.insert("scope_kind".into(), kind);
    m.insert("scope_value".into(), value);
    Ok(m)
}

pub(crate) fn create_limit_rule(rest: &Rest, r: &LimitRule) -> Result<()> {
    rest.put_doc(COLL, &r.id, &limit_fields(r)?)
}

pub(crate) fn list_limit_rules(
    rest: &Rest,
    project: &str,
    only_enabled: bool,
) -> Result<Vec<LimitRule>> {
    let mut filters: Vec<(&str, &str, Value)> = vec![("project_id", "EQUAL", json!(project))];
    if only_enabled {
        filters.push(("enabled", "EQUAL", json!(1_i64)));
    }
    let docs = rest.query(COLL, &filters, None, None)?;
    docs.iter().map(limit_from).collect()
}

pub(crate) fn get_limit_rule(
    rest: &Rest,
    project: Option<&str>,
    id: &str,
) -> Result<Option<LimitRule>> {
    let r = rest
        .get_doc(COLL, id)?
        .as_ref()
        .map(limit_from)
        .transpose()?;
    Ok(crate::scope::keep(project, r, |r| {
        Some(r.project_id.as_str())
    }))
}

/// Full-document replace, but only when the rule exists (the Store contract returns `false` for an
/// unknown id → API 404 — a plain put would silently create instead).
pub(crate) fn update_limit_rule(rest: &Rest, project: Option<&str>, r: &LimitRule) -> Result<bool> {
    if get_limit_rule(rest, project, &r.id)?.is_none() {
        return Ok(false);
    }
    rest.put_doc(COLL, &r.id, &limit_fields(r)?)?;
    Ok(true)
}

pub(crate) fn delete_limit_rule(rest: &Rest, project: Option<&str>, id: &str) -> Result<bool> {
    if get_limit_rule(rest, project, id)?.is_none() {
        return Ok(false);
    }
    rest.delete_doc(COLL, id)
}

fn limit_from(m: &Fields) -> Result<LimitRule> {
    Ok(LimitRule {
        id: freq(m, "id")?,
        project_id: freq(m, "project_id")?,
        metric: parse_enum("metric", &fstr(m, "metric").unwrap_or_default())?,
        window: parse_enum("window", &fstr(m, "window").unwrap_or_default())?,
        threshold: match fstr(m, "threshold_json") {
            Some(j) => serde_json::from_str(&j)?,
            None => Threshold::Fixed(ff64(m, "threshold").unwrap_or(0.0)),
        },
        action: parse_enum("action", &fstr(m, "action").unwrap_or_default())?,
        enabled: fbool(m, "enabled"),
        warn_at: ff64(m, "warn_at"),
        scope: match (fstr(m, "scope_kind"), fstr(m, "scope_value")) {
            (Some(kind), Some(value)) => LimitScope::from_parts(&kind, value),
            _ => None,
        },
        escalation: match fstr(m, "escalation_json") {
            Some(j) => Some(serde_json::from_str::<Escalation>(&j)?),
            None => None,
        },
        escalated_until: fstr(m, "escalated_until")
            .as_deref()
            .map(parse_ts)
            .transpose()?,
        origin: fstr(m, "origin"),
        expires_at: fstr(m, "expires_at").as_deref().map(parse_ts).transpose()?,
    })
}
