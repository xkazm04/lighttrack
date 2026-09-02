//! `Surface::Labels`: the human verdict ledger.
//!
//! The subject is stored as two columns (`subject_kind`, `subject_id`) rather than one `"kind:id"`
//! string, for the reason M9 split `scores.rubric`: a single column carrying several encodings is a
//! column nothing can index or join on, and the dataset join below is exactly a join.
//!
//! `dimensions` is a JSON blob read back whole and never filtered on — the same treatment
//! `scores.detail` gets.

use rusqlite::{params, Connection, ErrorCode, Row};

use lighttrack_core::{Label, LabelFilter, LabelSubject, ScoreDim};

use crate::codec::{decode_event_cursor, fmt_ts, parse_ts};
use crate::{Result, StoreError};

const COLS: &str = "id, project_id, subject_kind, subject_id, rubric_id, value, pass, \
     dimensions, labeler, note, created_at";

/// A duplicate label id is a [`StoreError::Conflict`] (409), never a silent overwrite: the row is
/// somebody's recorded opinion, and quietly replacing one with another is the one thing a ledger
/// must not do.
pub(super) fn insert(conn: &Connection, l: &Label) -> Result<()> {
    let dims = if l.dimensions.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&l.dimensions)?)
    };
    conn.execute(
        &format!("INSERT INTO labels ({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)"),
        params![
            l.id,
            l.project_id,
            l.subject.kind(),
            l.subject.id(),
            l.rubric_id,
            l.value,
            l.pass.map(|b| b as i64),
            dims,
            l.labeler,
            l.note,
            fmt_ts(l.created_at),
        ],
    )
    .map_err(|e| match &e {
        rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::ConstraintViolation => {
            StoreError::Conflict(format!("label '{}' already exists", l.id))
        }
        _ => e.into(),
    })?;
    Ok(())
}

/// Newest-first, keyset-paged on `(created_at, id)` — the same cursor codec the event listing uses,
/// so a page boundary inside a burst of same-instant labels neither skips nor repeats one.
pub(super) fn list(conn: &Connection, f: &LabelFilter) -> Result<Vec<Label>> {
    let mut sql = format!("SELECT {COLS} FROM labels WHERE 1=1");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(p) = &f.project {
        args.push(Box::new(p.clone()));
        sql.push_str(&format!(" AND project_id = ?{}", args.len()));
    }
    if let Some(s) = &f.subject {
        args.push(Box::new(s.kind().to_string()));
        sql.push_str(&format!(" AND subject_kind = ?{}", args.len()));
        args.push(Box::new(s.id().to_string()));
        sql.push_str(&format!(" AND subject_id = ?{}", args.len()));
    }
    if let Some(r) = &f.rubric_id {
        args.push(Box::new(r.clone()));
        sql.push_str(&format!(" AND rubric_id = ?{}", args.len()));
    }
    if let Some((ts, id)) = f.cursor.as_deref().and_then(decode_event_cursor) {
        args.push(Box::new(ts.clone()));
        let a = args.len();
        args.push(Box::new(ts));
        args.push(Box::new(id));
        sql.push_str(&format!(
            " AND (created_at < ?{a} OR (created_at = ?{} AND id < ?{}))",
            a + 1,
            a + 2
        ));
    }
    sql.push_str(&format!(
        " ORDER BY created_at DESC, id DESC LIMIT {}",
        f.effective_limit()
    ));
    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let raws = stmt
        .query_map(refs.as_slice(), map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().filter_map(transpose_row).collect()
}

/// Every label on any item of `dataset_id`, oldest-first so a calibration set is built in the
/// dataset's own order and kappa is independent of paging.
///
/// One statement with a subquery rather than a per-item lookup: a 500-case golden set is otherwise
/// 500 round trips, which is what makes "calibrate against the stored dataset" too slow to use and
/// keeps everyone on files.
pub(super) fn for_dataset(conn: &Connection, dataset_id: &str) -> Result<Vec<Label>> {
    let sql = format!(
        "SELECT {COLS} FROM labels \
         WHERE subject_kind = 'dataset_item' \
           AND subject_id IN (SELECT id FROM dataset_items WHERE dataset_id = ?1) \
         ORDER BY created_at ASC, id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![dataset_id], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().filter_map(transpose_row).collect()
}

type LabelRaw = (
    String,
    String,
    String,
    String,
    Option<String>,
    f64,
    Option<i64>,
    Option<String>,
    String,
    Option<String>,
    String,
);

fn map_raw(row: &Row) -> rusqlite::Result<LabelRaw> {
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
        row.get(9)?,
        row.get(10)?,
    ))
}

/// `None` for a subject kind this binary does not know — a newer writer's row, which is skipped
/// rather than misfiled as an event label. Anything else that fails to decode is a real error.
fn transpose_row(r: LabelRaw) -> Option<Result<Label>> {
    let subject = LabelSubject::from_parts(&r.2, &r.3)?;
    Some(from_raw(r, subject))
}

fn from_raw(r: LabelRaw, subject: LabelSubject) -> Result<Label> {
    Ok(Label {
        id: r.0,
        project_id: r.1,
        subject,
        rubric_id: r.4,
        value: r.5,
        pass: r.6.map(|v| v != 0),
        // An unreadable dimension blob degrades to "no breakdown" — the overall human score is
        // still true, and erroring would take the whole calibration set down with one bad row.
        dimensions: r
            .7
            .and_then(|s| serde_json::from_str::<Vec<ScoreDim>>(&s).ok())
            .unwrap_or_default(),
        labeler: r.8,
        note: r.9,
        created_at: parse_ts(&r.10)?,
    })
}
