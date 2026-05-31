//! `limit_rules` collection.

use serde_json::{json, Value};

use lighttrack_core::LimitRule;
use lighttrack_store::Result;

use crate::codec::*;
use crate::rest::Rest;

pub(crate) fn create_limit_rule(rest: &Rest, r: &LimitRule) -> Result<()> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(r.id));
    m.insert("project_id".into(), json!(r.project_id));
    m.insert("metric".into(), json!(enum_to_str(&r.metric)?));
    m.insert("window".into(), json!(enum_to_str(&r.window)?));
    m.insert("threshold".into(), json!(r.threshold));
    m.insert("action".into(), json!(enum_to_str(&r.action)?));
    m.insert("enabled".into(), json!(r.enabled as i64));
    rest.put_doc("limit_rules", &r.id, &m)
}

pub(crate) fn list_limit_rules(rest: &Rest, project: &str, only_enabled: bool) -> Result<Vec<LimitRule>> {
    let mut filters: Vec<(&str, &str, Value)> = vec![("project_id", "EQUAL", json!(project))];
    if only_enabled {
        filters.push(("enabled", "EQUAL", json!(1_i64)));
    }
    let docs = rest.query("limit_rules", &filters, None, None)?;
    docs.iter().map(limit_from).collect()
}

fn limit_from(m: &Fields) -> Result<LimitRule> {
    Ok(LimitRule {
        id: freq(m, "id")?,
        project_id: freq(m, "project_id")?,
        metric: parse_enum(&fstr(m, "metric").unwrap_or_default()),
        window: parse_enum(&fstr(m, "window").unwrap_or_default()),
        threshold: ff64(m, "threshold").unwrap_or(0.0),
        action: parse_enum(&fstr(m, "action").unwrap_or_default()),
        enabled: fbool(m, "enabled"),
    })
}
