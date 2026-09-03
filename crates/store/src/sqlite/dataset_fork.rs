//! Forking a dataset into its next version, and reading a name's version history (M24).

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};

use lighttrack_core::{new_id, Dataset, DatasetItem};

use super::datasets::{self, dataset_from_raw, map_dataset, DATASET_COLS};
use crate::{Result, StoreError};

/// Load a dataset, refusing one outside the scope as *absent* rather than as forbidden — the store
/// has no notion of a principal, and the API has already decided who may see what.
///
/// The scope is a predicate in the query (M17), not a post-hoc comparison: a row that is not this
/// tenant's is never read at all, so there is no branch left that could leak its existence.
pub(super) fn load_scoped(
    conn: &Connection,
    project: Option<&str>,
    id: &str,
) -> Result<Option<Dataset>> {
    let sql = format!(
        "SELECT {DATASET_COLS} FROM datasets WHERE id = ?1{}",
        super::scope_and(2)
    );
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt
        .query_row(params![id, project], map_dataset)
        .optional()?;
    raw.map(dataset_from_raw).transpose()
}

pub(super) fn fork(conn: &Connection, project: Option<&str>, id: &str) -> Result<Dataset> {
    let src = load_scoped(conn, project, id)?
        .ok_or_else(|| StoreError::Other(format!("dataset '{id}' not found")))?;

    // Past the highest version this NAME already carries, not past the source's own: forking v1
    // twice must not mint two v2s that a version pin can no longer tell apart.
    let next: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) + 1 FROM datasets WHERE project_id = ?1 AND name = ?2",
        params![src.project_id, src.name],
        |r| r.get(0),
    )?;

    let forked = Dataset {
        id: new_id(),
        project_id: src.project_id.clone(),
        name: src.name.clone(),
        version: next.max(1) as u32,
        // Unfrozen by construction: a fork exists to be extended. The source keeps its freeze, so
        // the run that was scored against it stays reproducible.
        frozen: false,
        source: src.source.clone(),
        created_at: Utc::now(),
        parent_id: Some(src.id.clone()),
    };

    // One transaction: a fork that created the row and then failed halfway through the copy would
    // leave a v2 that is a silent subset of its parent — the worst possible corpus, because it looks
    // complete and compares as if it were.
    let tx = conn.unchecked_transaction()?;
    datasets::create(conn, &forked)?;
    for item in datasets::list_items(conn, project, &src.id)? {
        let copy = DatasetItem {
            id: new_id(),
            dataset_id: forked.id.clone(),
            ..item.clone()
        };
        datasets::create_item(conn, &copy)?;
        copy_labels(conn, &item.id, &copy.id)?;
    }
    tx.commit()?;
    Ok(forked)
}

/// Carry every human verdict on a copied item forward onto its copy (M11).
///
/// Copied, never moved: the parent version is frozen evidence, and stripping its grades to furnish
/// the fork would rewrite the record a past calibration was measured on. A golden case whose label
/// did not survive the fork is not golden any more — it is an ungraded string, and the next
/// calibration would quietly measure the judge against fewer pairs while reporting the same corpus.
fn copy_labels(conn: &Connection, from_item: &str, to_item: &str) -> Result<()> {
    let mut stmt = conn
        .prepare("SELECT id FROM labels WHERE subject_kind = 'dataset_item' AND subject_id = ?1")?;
    let ids: Vec<String> = stmt
        .query_map(params![from_item], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for label_id in ids {
        conn.execute(
            "INSERT INTO labels \
             (id, project_id, subject_kind, subject_id, rubric_id, value, pass, dimensions, \
              labeler, note, created_at) \
             SELECT ?1, project_id, 'dataset_item', ?2, rubric_id, value, pass, dimensions, \
                    labeler, note, created_at \
             FROM labels WHERE id = ?3",
            params![new_id(), to_item, label_id],
        )?;
    }
    Ok(())
}

/// Every version of `name`, newest version first. An operator scope (`project = None`) reads
/// across every tenant; a project scope sees only its own.
pub(super) fn versions(
    conn: &Connection,
    project: Option<&str>,
    name: &str,
) -> Result<Vec<Dataset>> {
    let sql = format!(
        "SELECT {DATASET_COLS} FROM datasets WHERE name = ?2{} \
         ORDER BY version DESC, created_at DESC",
        super::scope_and(1)
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![project, name], map_dataset)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(dataset_from_raw).collect()
}
