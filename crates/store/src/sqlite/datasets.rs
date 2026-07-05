//! Datasets and dataset items.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

use lighttrack_core::{Dataset, DatasetItem};

use crate::codec::{fmt_ts, parse_ts};
use crate::Result;

const DATASET_COLS: &str = "id, project_id, name, version, frozen, source, created_at";
const ITEM_COLS: &str =
    "id, dataset_id, input, output, expected, context, tags, source_event_id, anonymization";

pub(super) fn create(conn: &Connection, d: &Dataset) -> Result<()> {
    conn.execute(
        "INSERT INTO datasets (id, project_id, name, version, frozen, source, created_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![d.id, d.project_id, d.name, d.version as i64, d.frozen as i64, d.source, fmt_ts(d.created_at)],
    )?;
    Ok(())
}

pub(super) fn get(conn: &Connection, id: &str) -> Result<Option<Dataset>> {
    let sql = format!("SELECT {DATASET_COLS} FROM datasets WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt.query_row(params![id], map_dataset).optional()?;
    raw.map(dataset_from_raw).transpose()
}

pub(super) fn list(conn: &Connection, project: &str) -> Result<Vec<Dataset>> {
    let sql =
        format!("SELECT {DATASET_COLS} FROM datasets WHERE project_id = ?1 ORDER BY created_at DESC");
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![project], map_dataset)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(dataset_from_raw).collect()
}

pub(super) fn set_frozen(conn: &Connection, id: &str, frozen: bool) -> Result<()> {
    conn.execute(
        "UPDATE datasets SET frozen = ?2 WHERE id = ?1",
        params![id, frozen as i64],
    )?;
    Ok(())
}

pub(super) fn create_item(conn: &Connection, item: &DatasetItem) -> Result<()> {
    let tags = serde_json::to_string(&item.tags)?;
    let anon = if item.anonymization.is_null() {
        None
    } else {
        Some(serde_json::to_string(&item.anonymization)?)
    };
    conn.execute(
        "INSERT INTO dataset_items \
         (id, dataset_id, input, output, expected, context, tags, source_event_id, anonymization) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            item.id, item.dataset_id, item.input, item.output, item.expected, item.context, tags,
            item.source_event_id, anon,
        ],
    )?;
    Ok(())
}

pub(super) fn list_items(conn: &Connection, dataset_id: &str) -> Result<Vec<DatasetItem>> {
    let sql = format!("SELECT {ITEM_COLS} FROM dataset_items WHERE dataset_id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![dataset_id], map_item)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(item_from_raw).collect()
}

type DatasetRaw = (String, String, String, i64, i64, Option<String>, String);

fn map_dataset(row: &Row) -> rusqlite::Result<DatasetRaw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn dataset_from_raw(r: DatasetRaw) -> Result<Dataset> {
    Ok(Dataset {
        id: r.0,
        project_id: r.1,
        name: r.2,
        version: r.3 as u32,
        frozen: r.4 != 0,
        source: r.5,
        created_at: parse_ts(&r.6)?,
    })
}

type ItemRaw = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn map_item(row: &Row) -> rusqlite::Result<ItemRaw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
    ))
}

fn item_from_raw(r: ItemRaw) -> Result<DatasetItem> {
    let tags: Vec<String> = match r.6 {
        Some(s) => serde_json::from_str(&s)?,
        None => Vec::new(),
    };
    let anonymization: Value = match r.8 {
        Some(s) => serde_json::from_str(&s)?,
        None => Value::Null,
    };
    Ok(DatasetItem {
        id: r.0,
        dataset_id: r.1,
        input: r.2,
        output: r.3,
        expected: r.4,
        context: r.5,
        tags,
        source_event_id: r.7,
        anonymization,
    })
}
