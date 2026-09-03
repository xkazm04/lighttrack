//! `rubrics` collection.

use serde_json::{json, Value};

use lighttrack_core::Rubric;
use lighttrack_store::Result;

use crate::codec::*;
use crate::rest::Rest;

pub(crate) fn create_rubric(rest: &Rest, r: &Rubric) -> Result<()> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(r.id));
    m.insert("project_id".into(), json!(r.project_id));
    m.insert("name".into(), json!(r.name));
    m.insert(
        "dimensions".into(),
        json!(serde_json::to_string(&r.dimensions)?),
    );
    m.insert("threshold".into(), json!(r.threshold));
    m.insert("created_at".into(), json!(fmt_ts(r.created_at)));
    // A rubric edit changes what a score means, so which generation this is and what it
    // replaces ride on the row (M9-C). A new version is a new document, never a mutation.
    m.insert("version".into(), json!(r.version as i64));
    m.insert("supersedes".into(), json!(r.supersedes));
    rest.put_doc("rubrics", &r.id, &m)
}

pub(crate) fn get_rubric(rest: &Rest, project: Option<&str>, id: &str) -> Result<Option<Rubric>> {
    let r = rest
        .get_doc("rubrics", id)?
        .as_ref()
        .map(rubric_from)
        .transpose()?;
    Ok(crate::scope::keep(project, r, |r| {
        Some(r.project_id.as_str())
    }))
}

pub(crate) fn list_rubrics(rest: &Rest, project: &str) -> Result<Vec<Rubric>> {
    let filters: Vec<(&str, &str, Value)> = vec![("project_id", "EQUAL", json!(project))];
    let docs = rest.query("rubrics", &filters, Some(("created_at", true)), None)?;
    docs.iter().map(rubric_from).collect()
}

fn rubric_from(m: &Fields) -> Result<Rubric> {
    let dims = freq(m, "dimensions")?;
    Ok(Rubric {
        id: freq(m, "id")?,
        project_id: freq(m, "project_id")?,
        name: freq(m, "name")?,
        dimensions: serde_json::from_str(&dims)?,
        threshold: ff64(m, "threshold").unwrap_or(0.7),
        created_at: parse_ts(&freq(m, "created_at")?)?,
        // Absent means generation 1, the same reading `Rubric`'s serde default takes.
        version: fi64(m, "version").unwrap_or(1).max(1) as u32,
        supersedes: fstr(m, "supersedes"),
    })
}
