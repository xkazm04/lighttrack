//! `prompts` + `prompt_versions` collections (the prompt registry).

use std::collections::BTreeMap;

use serde_json::{json, Value};

use lighttrack_core::{Prompt, PromptVersion};
use lighttrack_store::Result;

use crate::codec::*;
use crate::rest::Rest;

pub(crate) fn create_prompt(rest: &Rest, p: &Prompt) -> Result<()> {
    rest.put_doc("prompts", &p.id, &prompt_fields(p)?)
}

pub(crate) fn update_prompt(rest: &Rest, p: &Prompt) -> Result<()> {
    let mut m = Fields::new();
    m.insert("benchmark_id".into(), json!(p.benchmark_id));
    m.insert("labels".into(), json!(serde_json::to_string(&p.labels)?));
    m.insert("updated_at".into(), json!(fmt_ts(p.updated_at)));
    rest.patch_fields("prompts", &p.id, &m, &["benchmark_id", "labels", "updated_at"])
}

pub(crate) fn get_prompt(rest: &Rest, project: &str, name: &str) -> Result<Option<Prompt>> {
    let filters: Vec<(&str, &str, Value)> =
        vec![("project_id", "EQUAL", json!(project)), ("name", "EQUAL", json!(name))];
    let docs = rest.query("prompts", &filters, None, Some(1))?;
    docs.first().map(prompt_from).transpose()
}

pub(crate) fn get_prompt_by_id(rest: &Rest, id: &str) -> Result<Option<Prompt>> {
    rest.get_doc("prompts", id)?.as_ref().map(prompt_from).transpose()
}

pub(crate) fn list_prompts(rest: &Rest, project: &str) -> Result<Vec<Prompt>> {
    let filters: Vec<(&str, &str, Value)> = vec![("project_id", "EQUAL", json!(project))];
    let docs = rest.query("prompts", &filters, Some(("created_at", true)), None)?;
    docs.iter().map(prompt_from).collect()
}

pub(crate) fn create_prompt_version(rest: &Rest, v: &PromptVersion) -> Result<()> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(v.id));
    m.insert("prompt_id".into(), json!(v.prompt_id));
    m.insert("version".into(), json!(v.version as i64));
    m.insert("content".into(), json!(v.content));
    m.insert("config".into(), json!(json_or_null_str(&v.config)?));
    m.insert("note".into(), json!(v.note));
    m.insert("created_at".into(), json!(fmt_ts(v.created_at)));
    rest.put_doc("prompt_versions", &v.id, &m)
}

pub(crate) fn get_prompt_version(
    rest: &Rest,
    prompt_id: &str,
    version: u32,
) -> Result<Option<PromptVersion>> {
    let filters: Vec<(&str, &str, Value)> = vec![
        ("prompt_id", "EQUAL", json!(prompt_id)),
        ("version", "EQUAL", json!(version as i64)),
    ];
    let docs = rest.query("prompt_versions", &filters, None, Some(1))?;
    docs.first().map(version_from).transpose()
}

pub(crate) fn list_prompt_versions(rest: &Rest, prompt_id: &str) -> Result<Vec<PromptVersion>> {
    let filters: Vec<(&str, &str, Value)> = vec![("prompt_id", "EQUAL", json!(prompt_id))];
    let docs = rest.query("prompt_versions", &filters, Some(("version", true)), None)?;
    docs.iter().map(version_from).collect()
}

fn prompt_fields(p: &Prompt) -> Result<Fields> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(p.id));
    m.insert("project_id".into(), json!(p.project_id));
    m.insert("name".into(), json!(p.name));
    m.insert("benchmark_id".into(), json!(p.benchmark_id));
    m.insert("labels".into(), json!(serde_json::to_string(&p.labels)?));
    m.insert("created_at".into(), json!(fmt_ts(p.created_at)));
    m.insert("updated_at".into(), json!(fmt_ts(p.updated_at)));
    Ok(m)
}

fn prompt_from(m: &Fields) -> Result<Prompt> {
    let labels: BTreeMap<String, u32> = match fstr(m, "labels") {
        Some(s) => serde_json::from_str(&s)?,
        None => BTreeMap::new(),
    };
    Ok(Prompt {
        id: freq(m, "id")?,
        project_id: freq(m, "project_id")?,
        name: freq(m, "name")?,
        benchmark_id: fstr(m, "benchmark_id"),
        labels,
        created_at: parse_ts(&freq(m, "created_at")?)?,
        updated_at: parse_ts(&freq(m, "updated_at")?)?,
    })
}

fn version_from(m: &Fields) -> Result<PromptVersion> {
    Ok(PromptVersion {
        id: freq(m, "id")?,
        prompt_id: freq(m, "prompt_id")?,
        version: fi64(m, "version").unwrap_or(0) as u32,
        content: freq(m, "content")?,
        config: fjson(m, "config")?,
        note: fstr(m, "note"),
        created_at: parse_ts(&freq(m, "created_at")?)?,
    })
}
