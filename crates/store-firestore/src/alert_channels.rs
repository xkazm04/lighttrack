//! `alert_channels` collection: create / get / list / delete.
//!
//! Same shape as SQLite and Postgres. A global channel is `project_id` absent — Firestore has no
//! `NULL` to filter on, so the row carries an explicit `global` boolean and the two listings query
//! that, which keeps "the project's own" and "the deployment's" genuinely disjoint.

use serde_json::json;

use lighttrack_core::{AlertChannel, AlertKind, ChannelKind, Severity};
use lighttrack_store::codec::{fmt_ts, parse_ts};
use lighttrack_store::{Result, StoreError};

use crate::codec::*;
use crate::rest::Rest;

const COLL: &str = "alert_channels";

fn channel_fields(c: &AlertChannel) -> Result<Fields> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(c.id));
    m.insert("project_id".into(), json!(c.project_id));
    // Firestore cannot filter on "field is absent", so global-ness is a value, not an absence.
    m.insert("global".into(), json!(c.project_id.is_none()));
    m.insert("kind".into(), json!(c.kind.as_str()));
    m.insert("target".into(), json!(c.target));
    m.insert("secret_hash".into(), json!(c.secret_hash));
    m.insert("prev_secret_hash".into(), json!(c.prev_secret_hash));
    m.insert("min_severity".into(), json!(c.min_severity.as_str()));
    m.insert("kinds".into(), json!(serde_json::to_string(&c.kinds)?));
    m.insert("enabled".into(), json!(c.enabled));
    m.insert("created_at".into(), json!(fmt_ts(c.created_at)));
    Ok(m)
}

pub(crate) fn create_alert_channel(rest: &Rest, c: &AlertChannel) -> Result<()> {
    rest.put_doc(COLL, &c.id, &channel_fields(c)?)
}

pub(crate) fn get_alert_channel(rest: &Rest, id: &str) -> Result<Option<AlertChannel>> {
    rest.get_doc(COLL, id)?
        .as_ref()
        .map(channel_from)
        .transpose()
}

pub(crate) fn list_alert_channels(rest: &Rest, project: Option<&str>) -> Result<Vec<AlertChannel>> {
    let filters: Vec<(&str, &str, serde_json::Value)> = match project {
        Some(p) => vec![("project_id", "EQUAL", json!(p))],
        None => vec![("global", "EQUAL", json!(true))],
    };
    let docs = rest.query(COLL, &filters, None, None)?;
    docs.iter().map(channel_from).collect()
}

pub(crate) fn delete_alert_channel(rest: &Rest, id: &str) -> Result<bool> {
    rest.delete_doc(COLL, id)
}

fn channel_from(m: &Fields) -> Result<AlertChannel> {
    let id: String = freq(m, "id")?;
    let kind_raw = fstr(m, "kind").unwrap_or_default();
    let kind = ChannelKind::from_wire(&kind_raw).ok_or_else(|| {
        StoreError::Other(format!(
            "alert channel '{id}' carries an unknown kind '{kind_raw}'"
        ))
    })?;
    let created_at = fstr(m, "created_at").ok_or_else(|| missing("created_at"))?;
    Ok(AlertChannel {
        id,
        project_id: fstr(m, "project_id"),
        kind,
        target: fstr(m, "target").unwrap_or_default(),
        secret_hash: fstr(m, "secret_hash"),
        prev_secret_hash: fstr(m, "prev_secret_hash"),
        min_severity: Severity::from_wire(&fstr(m, "min_severity").unwrap_or_default()),
        // A kind this build does not know is dropped from the *filter*, not from the channel.
        kinds: fstr(m, "kinds")
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .map(|v| v.iter().filter_map(|s| AlertKind::from_wire(s)).collect())
            .unwrap_or_default(),
        enabled: fbool(m, "enabled"),
        created_at: parse_ts(&created_at)?,
    })
}
