//! Datasets and their items.

use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{Dataset, DatasetItem};
use lighttrack_store::Result;

use crate::util::{fmt_ts, json_or_null, parse_ts, pgerr, val_or_null};

pub(crate) const DATASET_COLS: &str =
    "id, project_id, name, version, frozen, source, created_at, parent_id";

const ITEM_COLS: &str = "id, dataset_id, input, output, expected, context, tags, \
    source_event_id, anonymization, input_hash";

pub(crate) async fn create(pool: &PgPool, d: &Dataset) -> Result<()> {
    sqlx::query(
        "INSERT INTO datasets \
         (id, project_id, name, version, frozen, source, created_at, parent_id) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(d.id.clone())
    .bind(d.project_id.clone())
    .bind(d.name.clone())
    .bind(d.version as i64)
    .bind(d.frozen as i64)
    .bind(d.source.clone())
    .bind(fmt_ts(d.created_at))
    .bind(d.parent_id.clone())
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn get(pool: &PgPool, project: Option<&str>, id: &str) -> Result<Option<Dataset>> {
    let row = sqlx::query(&format!(
        "SELECT {DATASET_COLS} FROM datasets WHERE id = $1 AND ($2::text IS NULL OR project_id = $2)"
    ))
    .bind(id.to_string())
    .bind(project.map(str::to_string))
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    row.as_ref().map(dataset_from_row).transpose()
}

pub(crate) async fn list(pool: &PgPool, project: Option<&str>) -> Result<Vec<Dataset>> {
    let rows = sqlx::query(&format!(
        "SELECT {DATASET_COLS} FROM datasets \
         WHERE ($1::text IS NULL OR project_id = $1) ORDER BY created_at DESC"
    ))
    .bind(project.map(str::to_string))
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(dataset_from_row).collect()
}

pub(crate) async fn set_frozen(
    pool: &PgPool,
    project: Option<&str>,
    id: &str,
    frozen: bool,
) -> Result<()> {
    sqlx::query(
        "UPDATE datasets SET frozen = $2 WHERE id = $1 AND ($3::text IS NULL OR project_id = $3)",
    )
    .bind(id.to_string())
    .bind(frozen as i64)
    .bind(project.map(str::to_string))
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn create_item(pool: &PgPool, item: &DatasetItem) -> Result<()> {
    let tags = serde_json::to_string(&item.tags)?;
    let anon = json_or_null(&item.anonymization)?;
    sqlx::query(
        "INSERT INTO dataset_items (id, dataset_id, input, output, expected, context, \
         tags, source_event_id, anonymization, input_hash) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(item.id.clone())
    .bind(item.dataset_id.clone())
    .bind(item.input.clone())
    .bind(item.output.clone())
    .bind(item.expected.clone())
    .bind(item.context.clone())
    .bind(tags)
    .bind(item.source_event_id.clone())
    .bind(anon)
    .bind(item.input_hash.clone())
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn list_items(
    pool: &PgPool,
    project: Option<&str>,
    dataset_id: &str,
) -> Result<Vec<DatasetItem>> {
    let rows = sqlx::query(&format!(
        "SELECT {ITEM_COLS} FROM dataset_items WHERE dataset_id = $1 \
           AND ($2::text IS NULL OR EXISTS \
                (SELECT 1 FROM datasets d WHERE d.id = dataset_id AND d.project_id = $2))"
    ))
    .bind(dataset_id.to_string())
    .bind(project.map(str::to_string))
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(item_from_row).collect()
}

pub(crate) fn dataset_from_row(row: &PgRow) -> Result<Dataset> {
    let created_at: String = row.try_get(6).map_err(pgerr)?;
    Ok(Dataset {
        id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        name: row.try_get(2).map_err(pgerr)?,
        version: row.try_get::<i64, _>(3).map_err(pgerr)? as u32,
        frozen: row.try_get::<i64, _>(4).map_err(pgerr)? != 0,
        source: row.try_get(5).map_err(pgerr)?,
        created_at: parse_ts(&created_at)?,
        parent_id: row.try_get(7).map_err(pgerr)?,
    })
}

fn item_from_row(row: &PgRow) -> Result<DatasetItem> {
    let tags: Option<String> = row.try_get(6).map_err(pgerr)?;
    let anon: Option<String> = row.try_get(8).map_err(pgerr)?;
    Ok(DatasetItem {
        id: row.try_get(0).map_err(pgerr)?,
        dataset_id: row.try_get(1).map_err(pgerr)?,
        input: row.try_get(2).map_err(pgerr)?,
        output: row.try_get(3).map_err(pgerr)?,
        expected: row.try_get(4).map_err(pgerr)?,
        context: row.try_get(5).map_err(pgerr)?,
        tags: match tags {
            Some(s) => serde_json::from_str(&s)?,
            None => Vec::new(),
        },
        source_event_id: row.try_get(7).map_err(pgerr)?,
        anonymization: val_or_null(anon)?,
        input_hash: row.try_get(9).map_err(pgerr)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_cols_match_the_schema_model() {
        use lighttrack_store::schema::{tables, Dialect};
        assert_eq!(
            DATASET_COLS,
            tables::DATASETS.select_list(Dialect::Postgres)
        );
    }

    #[test]
    fn item_cols_match_the_schema_model() {
        use lighttrack_store::schema::{tables, Dialect};
        assert_eq!(
            ITEM_COLS,
            tables::DATASET_ITEMS.select_list(Dialect::Postgres)
        );
    }
}
