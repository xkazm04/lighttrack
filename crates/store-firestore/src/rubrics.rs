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
    m.insert("dimensions".into(), json!(serde_json::to_string(&r.dimensions)?));
    m.insert("threshold".into(), json!(r.threshold));
    m.insert("created_at".into(), json!(fmt_ts(r.created_at)));
    rest.put_doc("rubrics", &r.id, &m)
}

pub(crate) fn get_rubric(rest: &Rest, id: &str) -> Result<Option<Rubric>> {
    rest.get_doc("rubrics", id)?.as_ref().map(rubric_from).transpose()
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
    })
}
