//! `Surface::Labels`: the human verdict ledger (M11).
//!
//! The subject is two columns (`subject_kind`, `subject_id`) rather than one `"kind:id"` string, so
//! the dataset read below can be a real join instead of one query per item — a 500-case golden set
//! is otherwise 500 round trips, which is what keeps everybody on files.

use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{Label, LabelFilter, LabelSubject, ScoreDim};
use lighttrack_store::{Result, StoreError};

use lighttrack_store::codec::decode_event_cursor;

use crate::util::{fmt_ts, parse_ts, pgerr};

const COLS: &str = "id, project_id, subject_kind, subject_id, rubric_id, value, pass, \
     dimensions, labeler, note, created_at";

pub(crate) async fn insert(pool: &PgPool, l: &Label) -> Result<()> {
    let dims = if l.dimensions.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&l.dimensions)?)
    };
    sqlx::query(&format!(
        "INSERT INTO labels ({COLS}) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)"
    ))
    .bind(l.id.clone())
    .bind(l.project_id.clone())
    .bind(l.subject.kind())
    .bind(l.subject.id().to_string())
    .bind(l.rubric_id.clone())
    .bind(l.value)
    .bind(l.pass)
    .bind(dims)
    .bind(l.labeler.clone())
    .bind(l.note.clone())
    .bind(fmt_ts(l.created_at))
    .execute(pool)
    .await
    .map_err(insert_err(&l.id))?;
    Ok(())
}

/// A duplicate id is a [`StoreError::Conflict`] (409), never a silent overwrite: the row is
/// somebody's recorded opinion.
fn insert_err(id: &str) -> impl Fn(sqlx::Error) -> StoreError + '_ {
    move |e| match &e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
            StoreError::Conflict(format!("label '{id}' already exists"))
        }
        _ => pgerr(e),
    }
}

/// Newest-first, keyset-paged on `(created_at, id)`.
pub(crate) async fn list(pool: &PgPool, f: &LabelFilter) -> Result<Vec<Label>> {
    let mut sql = format!("SELECT {COLS} FROM labels WHERE 1=1");
    let mut binds: Vec<String> = Vec::new();
    if let Some(p) = &f.project {
        binds.push(p.clone());
        sql.push_str(&format!(" AND project_id = ${}", binds.len()));
    }
    if let Some(s) = &f.subject {
        binds.push(s.kind().to_string());
        sql.push_str(&format!(" AND subject_kind = ${}", binds.len()));
        binds.push(s.id().to_string());
        sql.push_str(&format!(" AND subject_id = ${}", binds.len()));
    }
    if let Some(r) = &f.rubric_id {
        binds.push(r.clone());
        sql.push_str(&format!(" AND rubric_id = ${}", binds.len()));
    }
    if let Some((ts, id)) = f.cursor.as_deref().and_then(decode_event_cursor) {
        binds.push(ts.clone());
        let a = binds.len();
        binds.push(ts);
        binds.push(id);
        sql.push_str(&format!(
            " AND (created_at < ${a} OR (created_at = ${} AND id < ${}))",
            a + 1,
            a + 2
        ));
    }
    sql.push_str(&format!(
        " ORDER BY created_at DESC, id DESC LIMIT {}",
        f.effective_limit()
    ));
    let mut q = sqlx::query(&sql);
    for b in binds {
        q = q.bind(b);
    }
    let rows = q.fetch_all(pool).await.map_err(pgerr)?;
    rows.iter().filter_map(transpose_row).collect()
}

/// Every label on any item of `dataset_id`, oldest-first so the calibration set is built in the
/// dataset's own order and κ does not depend on paging.
pub(crate) async fn for_dataset(
    pool: &PgPool,
    project: Option<&str>,
    dataset_id: &str,
) -> Result<Vec<Label>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM labels \
         WHERE subject_kind = 'dataset_item' \
           AND subject_id IN (SELECT id FROM dataset_items WHERE dataset_id = $1) AND ($2::text IS NULL OR project_id = $2) \
         ORDER BY created_at ASC, id ASC"
    ))
    .bind(dataset_id.to_string())
    .bind(project.map(str::to_string))
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().filter_map(transpose_row).collect()
}

/// `None` for a subject kind this binary does not know — a newer writer's row, skipped rather than
/// misfiled as an event label.
fn transpose_row(row: &PgRow) -> Option<Result<Label>> {
    let kind: String = row.try_get(2).ok()?;
    let id: String = row.try_get(3).ok()?;
    let subject = LabelSubject::from_parts(&kind, &id)?;
    Some(from_row(row, subject))
}

fn from_row(row: &PgRow, subject: LabelSubject) -> Result<Label> {
    let dims: Option<String> = row.try_get(7).map_err(pgerr)?;
    let created_at: String = row.try_get(10).map_err(pgerr)?;
    Ok(Label {
        id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        subject,
        rubric_id: row.try_get(4).map_err(pgerr)?,
        value: row.try_get(5).map_err(pgerr)?,
        pass: row.try_get(6).map_err(pgerr)?,
        // An unreadable breakdown degrades to "no breakdown" — the overall human score is still
        // true, and erroring would take a whole calibration set down with one bad row.
        dimensions: dims
            .and_then(|s| serde_json::from_str::<Vec<ScoreDim>>(&s).ok())
            .unwrap_or_default(),
        labeler: row.try_get(8).map_err(pgerr)?,
        note: row.try_get(9).map_err(pgerr)?,
        created_at: parse_ts(&created_at)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::select_list_names;

    /// `from_row` reads by position, so the `COLS` order is load-bearing: a column inserted in the
    /// middle would silently shift every read one to the left.
    #[test]
    fn the_select_list_matches_the_positional_reads() {
        assert_eq!(
            select_list_names(COLS),
            vec![
                "id",
                "project_id",
                "subject_kind",
                "subject_id",
                "rubric_id",
                "value",
                "pass",
                "dimensions",
                "labeler",
                "note",
                "created_at"
            ]
        );
    }
}
