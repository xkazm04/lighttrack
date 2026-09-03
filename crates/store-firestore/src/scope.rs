//! Applying a tenant scope on the Firestore backend.
//!
//! Two shapes, because Firestore has two kinds of read. A **query** takes the scope as one more
//! `EQUAL` filter, exactly like the SQL backends' `AND project_id = ?`. A **point read** fetches a
//! document by its id and cannot carry a filter at all, so the predicate runs here — inside the
//! store, before the row is handed back. The property D13 asks for is the same either way: a
//! foreign id reads as `None`, so no 404 ever confirms that someone else's row exists and no
//! handler has to compensate with a 403.

use serde_json::{json, Value};

/// Whether a row owned by `owner` is visible to `project`. `None` for `project` is the operator,
/// which sees everything including the project-less rows; `None` for `owner` is such a row.
pub(crate) fn allows(project: Option<&str>, owner: Option<&str>) -> bool {
    match project {
        None => true,
        Some(p) => owner == Some(p),
    }
}

/// The point-read form: drop a fetched row the scope may not see.
pub(crate) fn keep<T>(
    project: Option<&str>,
    row: Option<T>,
    owner: impl Fn(&T) -> Option<&str>,
) -> Option<T> {
    row.filter(|r| allows(project, owner(r)))
}

/// The query form: one more `EQUAL` filter when the scope is a tenant, nothing when it is the
/// operator (who reads every project's rows *and* the project-less ones, which an `EQUAL` on a
/// missing field would exclude).
pub(crate) fn push_filter<'a>(filters: &mut Vec<(&'a str, &'a str, Value)>, project: Option<&str>) {
    if let Some(p) = project {
        filters.push(("project_id", "EQUAL", json!(p)));
    }
}
