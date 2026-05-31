//! `datasets` + `dataset_items` collections.

use serde_json::{json, Value};

use lighttrack_core::{Dataset, DatasetItem};
use lighttrack_store::Result;

use crate::codec::*;
use crate::rest::Rest;

pub(crate) fn create_dataset(rest: &Rest, d: &Dataset) -> Result<()> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(d.id));
    m.insert("project_id".into(), json!(d.project_id));
    m.insert("name".into(), json!(d.name));
    m.insert("version".into(), json!(d.version as i64));
    m.insert("frozen".into(), json!(d.frozen as i64));
    m.insert("source".into(), json!(d.source));
    m.insert("created_at".into(), json!(fmt_ts(d.created_at)));
    rest.put_doc("datasets", &d.id, &m)
}

pub(crate) fn get_dataset(rest: &Rest, id: &str) -> Result<Option<Dataset>> {
    rest.get_doc("datasets", id)?.as_ref().map(dataset_from).transpose()
}

pub(crate) fn list_datasets(rest: &Rest, project: &str) -> Result<Vec<Dataset>> {
    let filters: Vec<(&str, &str, Value)> = vec![("project_id", "EQUAL", json!(project))];
    let docs = rest.query("datasets", &filters, Some(("created_at", true)), None)?;
    docs.iter().map(dataset_from).collect()
}

pub(crate) fn set_dataset_frozen(rest: &Rest, id: &str, frozen: bool) -> Result<()> {
    let mut m = Fields::new();
    m.insert("frozen".into(), json!(frozen as i64));
    rest.patch_fields("datasets", id, &m, &["frozen"])
}

pub(crate) fn create_dataset_item(rest: &Rest, item: &DatasetItem) -> Result<()> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(item.id));
    m.insert("dataset_id".into(), json!(item.dataset_id));
    m.insert("input".into(), json!(item.input));
    m.insert("output".into(), json!(item.output));
    m.insert("expected".into(), json!(item.expected));
    m.insert("context".into(), json!(item.context));
    m.insert("tags".into(), json!(serde_json::to_string(&item.tags)?));
    m.insert("source_event_id".into(), json!(item.source_event_id));
    m.insert("anonymization".into(), json!(json_or_null_str(&item.anonymization)?));
    rest.put_doc("dataset_items", &item.id, &m)
}

pub(crate) fn list_dataset_items(rest: &Rest, dataset_id: &str) -> Result<Vec<DatasetItem>> {
    let filters: Vec<(&str, &str, Value)> = vec![("dataset_id", "EQUAL", json!(dataset_id))];
    let docs = rest.query("dataset_items", &filters, None, None)?;
    docs.iter().map(item_from).collect()
}

fn dataset_from(m: &Fields) -> Result<Dataset> {
    Ok(Dataset {
        id: freq(m, "id")?,
        project_id: freq(m, "project_id")?,
        name: freq(m, "name")?,
        version: fi64(m, "version").unwrap_or(1) as u32,
        frozen: fbool(m, "frozen"),
        source: fstr(m, "source"),
        created_at: parse_ts(&freq(m, "created_at")?)?,
    })
}

fn item_from(m: &Fields) -> Result<DatasetItem> {
    Ok(DatasetItem {
        id: freq(m, "id")?,
        dataset_id: freq(m, "dataset_id")?,
        input: freq(m, "input")?,
        output: fstr(m, "output"),
        expected: fstr(m, "expected"),
        context: fstr(m, "context"),
        tags: match fstr(m, "tags") {
            Some(s) => serde_json::from_str(&s)?,
            None => Vec::new(),
        },
        source_event_id: fstr(m, "source_event_id"),
        anonymization: fjson(m, "anonymization")?,
    })
}
