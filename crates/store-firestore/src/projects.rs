//! `projects` + `api_keys` collections.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use lighttrack_core::{decode_scopes, encode_scopes, ApiKey, Project, Redaction};
use lighttrack_store::Result;

use crate::codec::*;
use crate::rest::Rest;

pub(crate) fn create_project(rest: &Rest, p: &Project) -> Result<()> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(p.id));
    m.insert("name".into(), json!(p.name));
    m.insert("enabled".into(), json!(p.enabled as i64));
    m.insert("redaction".into(), json!(enum_to_str(&p.redaction)?));
    m.insert(
        "collective_opt_in".into(),
        json!(p.collective_opt_in as i64),
    );
    m.insert(
        "require_trusted_judge".into(),
        json!(p.require_trusted_judge as i64),
    );
    m.insert("created_at".into(), json!(fmt_ts(p.created_at)));
    m.insert("archived_at".into(), json!(p.archived_at.map(fmt_ts)));
    rest.put_doc("projects", &p.id, &m)
}

pub(crate) fn get_project(rest: &Rest, id: &str) -> Result<Option<Project>> {
    rest.get_doc("projects", id)?
        .as_ref()
        .map(project_from)
        .transpose()
}

pub(crate) fn list_projects(rest: &Rest) -> Result<Vec<Project>> {
    let docs = rest.query("projects", &[], Some(("created_at", true)), None)?;
    docs.iter().map(project_from).collect()
}

pub(crate) fn create_api_key(rest: &Rest, k: &ApiKey) -> Result<()> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(k.id));
    m.insert("project_id".into(), json!(k.project_id));
    m.insert("name".into(), json!(k.name));
    m.insert("prefix".into(), json!(k.prefix));
    m.insert("key_hash".into(), json!(k.key_hash));
    m.insert("created_at".into(), json!(fmt_ts(k.created_at)));
    m.insert("last_used_at".into(), json!(k.last_used_at.map(fmt_ts)));
    m.insert("revoked".into(), json!(k.revoked as i64));
    m.insert("scopes".into(), json!(encode_scopes(&k.scopes)));
    m.insert("expires_at".into(), json!(k.expires_at.map(fmt_ts)));
    rest.put_doc("api_keys", &k.id, &m)
}

pub(crate) fn find_api_key_by_prefix(rest: &Rest, prefix: &str) -> Result<Option<ApiKey>> {
    let filters: Vec<(&str, &str, Value)> = vec![("prefix", "EQUAL", json!(prefix))];
    let docs = rest.query("api_keys", &filters, None, Some(1))?;
    docs.first().map(api_key_from).transpose()
}

pub(crate) fn touch_api_key(rest: &Rest, id: &str, when: DateTime<Utc>) -> Result<()> {
    let mut m = Fields::new();
    m.insert("last_used_at".into(), json!(fmt_ts(when)));
    rest.patch_fields("api_keys", id, &m, &["last_used_at"])
}

pub(crate) fn list_api_keys(rest: &Rest, project: &str) -> Result<Vec<ApiKey>> {
    let filters: Vec<(&str, &str, Value)> = vec![("project_id", "EQUAL", json!(project))];
    let docs = rest.query("api_keys", &filters, Some(("created_at", true)), None)?;
    docs.iter().map(api_key_from).collect()
}

pub(crate) fn set_api_key_revoked(rest: &Rest, id: &str, revoked: bool) -> Result<bool> {
    // No cheap existence probe on a single-doc PATCH; a get_doc first lets us return the honest
    // "unknown id" (false) the API maps to 404, rather than silently succeeding on a typo.
    if rest.get_doc("api_keys", id)?.is_none() {
        return Ok(false);
    }
    let mut m = Fields::new();
    m.insert("revoked".into(), json!(revoked as i64));
    rest.patch_fields("api_keys", id, &m, &["revoked"])?;
    Ok(true)
}

/// Stamp (or clear) a key's expiry — the durable half of a rotation's grace window. Same
/// existence probe as [`set_api_key_revoked`]: a PATCH alone cannot tell "written" from "typo".
pub(crate) fn set_api_key_expiry(
    rest: &Rest,
    id: &str,
    when: Option<DateTime<Utc>>,
) -> Result<bool> {
    if rest.get_doc("api_keys", id)?.is_none() {
        return Ok(false);
    }
    let mut m = Fields::new();
    m.insert("expires_at".into(), json!(when.map(fmt_ts)));
    rest.patch_fields("api_keys", id, &m, &["expires_at"])?;
    Ok(true)
}

fn project_from(m: &Fields) -> Result<Project> {
    Ok(Project {
        id: freq(m, "id")?,
        name: freq(m, "name")?,
        enabled: fbool(m, "enabled"),
        redaction: parse_enum::<Redaction>("redaction", &fstr(m, "redaction").unwrap_or_default())?,
        // Docs written before the consent field existed read as opted OUT — the safe default.
        collective_opt_in: fbool(m, "collective_opt_in"),
        // Docs written before the judge-trust policy existed read as OFF — an upgrade must not
        // start blocking gates that were passing yesterday.
        require_trusted_judge: fbool(m, "require_trusted_judge"),
        created_at: parse_ts(&freq(m, "created_at")?)?,
        archived_at: match fstr(m, "archived_at") {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
    })
}

fn api_key_from(m: &Fields) -> Result<ApiKey> {
    Ok(ApiKey {
        id: freq(m, "id")?,
        project_id: freq(m, "project_id")?,
        name: freq(m, "name")?,
        prefix: freq(m, "prefix")?,
        key_hash: freq(m, "key_hash")?,
        created_at: parse_ts(&freq(m, "created_at")?)?,
        last_used_at: match fstr(m, "last_used_at") {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        revoked: fbool(m, "revoked"),
        // Docs written before scopes existed carry no field, which `decode_scopes` reads as the
        // permissive back-compat default rather than locking a live key out on upgrade.
        scopes: decode_scopes(fstr(m, "scopes").as_deref()),
        expires_at: match fstr(m, "expires_at") {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
    })
}
