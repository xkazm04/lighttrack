//! The M14 non-negotiable: the model-rendered SQLite schema must produce **exactly** the database
//! the hand-written one did.
//!
//! Both paths are run against a fresh in-memory database — the frozen pre-M14 apply path from
//! `tests/legacy/` on one, `SqliteStore`'s live path on the other — and the resulting schemas are
//! compared column by column and index by index.
//!
//! Physical column *order* is deliberately not compared. It was never stable: a column added by
//! `ALTER` sits at the end of an upgraded database and inline on a freshly-created one, so the two
//! disagreed long before this refactor. What must agree is the set of columns, each one's type,
//! nullability, default and key membership; the primary key's own column order; and the indexes.

#[path = "legacy/mod.rs"]
mod legacy;

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

/// One column as a reader observes it: `PRAGMA table_info` minus the physical position.
type Col = (String, String, i64, Option<String>, i64);

fn columns(c: &Connection, table: &str) -> BTreeMap<String, Col> {
    let mut stmt = c
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("table_info");
    stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, i64>(5)?,
        ))
    })
    .expect("map")
    .collect::<rusqlite::Result<Vec<Col>>>()
    .expect("rows")
    .into_iter()
    .map(|c| (c.0.clone(), c))
    .collect()
}

/// The primary key, in key order — the one place physical order is load-bearing.
fn primary_key(c: &Connection, table: &str) -> Vec<String> {
    let mut stmt = c
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("table_info");
    let mut rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(5)?, r.get::<_, String>(1)?)))
        .expect("map")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("rows")
        .into_iter()
        .filter(|(k, _)| *k > 0)
        .collect();
    rows.sort_by_key(|(k, _)| *k);
    rows.into_iter().map(|(_, n)| n).collect()
}

fn tables(c: &Connection) -> BTreeSet<String> {
    let mut stmt = c
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .expect("prepare");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("map")
        .collect::<rusqlite::Result<BTreeSet<_>>>()
        .expect("rows")
}

/// Index name → (unique, partial, the columns it covers, in index order).
fn indexes(c: &Connection) -> BTreeMap<String, (i64, i64, Vec<String>)> {
    let mut out = BTreeMap::new();
    for t in tables(c) {
        let mut stmt = c
            .prepare(&format!("PRAGMA index_list({t})"))
            .expect("index_list");
        let listed: Vec<(String, i64, i64)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(4).unwrap_or(0),
                ))
            })
            .expect("map")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows");
        for (name, unique, partial) in listed {
            if name.starts_with("sqlite_autoindex") {
                continue;
            }
            let mut istmt = c
                .prepare(&format!("PRAGMA index_info({name})"))
                .expect("index_info");
            let cols: Vec<String> = istmt
                .query_map([], |r| r.get::<_, Option<String>>(2))
                .expect("map")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("rows")
                .into_iter()
                .map(|c| c.unwrap_or_else(|| "<expr>".into()))
                .collect();
            out.insert(name, (unique, partial, cols));
        }
    }
    out
}

/// Apply the pre-M14 path verbatim: the columns before the batch, the batch, then the columns again
/// plus the late list — and the M26 rebuild, which ran first in the old backend too.
fn apply_legacy(c: &Connection) {
    let add = |stmt: &str| match c.execute(stmt, []) {
        Ok(_) => true,
        Err(e) => {
            let m = e.to_string();
            assert!(
                m.contains("duplicate column name") || m.contains("no such table"),
                "unexpected error applying {stmt}: {m}"
            );
            false
        }
    };
    for stmt in legacy::LEGACY_ADDED_COLUMNS {
        add(stmt);
    }
    add(legacy::LEGACY_ADD_RECEIVED_AT);
    c.execute_batch(legacy::LEGACY_SCHEMA).expect("batch");
    for stmt in legacy::LEGACY_ADDED_COLUMNS
        .iter()
        .chain(legacy::LEGACY_ADDED_COLUMNS_LATE)
    {
        add(stmt);
    }
    // The legacy backend rebuilt `model_prices` when it lacked `effective_from`, which is always
    // true of the shape `LEGACY_SCHEMA` just created.
    c.execute_batch(legacy::LEGACY_M26_REBUILD)
        .expect("m26 rebuild");
}

fn legacy_db() -> Connection {
    let c = Connection::open_in_memory().expect("db");
    apply_legacy(&c);
    c
}

fn rendered_db() -> Connection {
    let c = Connection::open_in_memory().expect("db");
    for stmt in lighttrack_store::schema::plan(lighttrack_store::schema::Dialect::Sqlite) {
        if let Err(e) = c.execute_batch(&stmt) {
            let m = e.to_string();
            assert!(
                m.contains("duplicate column name") || m.contains("no such table"),
                "unexpected error applying {stmt}: {m}"
            );
        }
    }
    c
}

#[test]
fn the_rendered_schema_creates_the_same_tables() {
    assert_eq!(
        tables(&rendered_db()),
        tables(&legacy_db()),
        "the model must declare exactly the tables the shipped schema created"
    );
}

#[test]
fn the_rendered_schema_creates_the_same_columns() {
    let (new, old) = (rendered_db(), legacy_db());
    for t in tables(&old) {
        assert_eq!(
            columns(&new, &t),
            columns(&old, &t),
            "table `{t}`: the rendered schema does not match the shipped one"
        );
        assert_eq!(
            primary_key(&new, &t),
            primary_key(&old, &t),
            "table `{t}`: primary key differs"
        );
    }
}

#[test]
fn the_rendered_schema_creates_the_same_indexes() {
    assert_eq!(
        indexes(&rendered_db()),
        indexes(&legacy_db()),
        "an index was gained, lost or redefined"
    );
}

/// Idempotency, the local stand-in for the env-gated Postgres run: applying the plan to a database
/// that already has it must change nothing.
#[test]
fn re_applying_the_plan_changes_nothing() {
    let c = rendered_db();
    let before = (tables(&c), indexes(&c));
    for stmt in lighttrack_store::schema::plan(lighttrack_store::schema::Dialect::Sqlite) {
        let _ = c.execute_batch(&stmt);
    }
    assert_eq!(before, (tables(&c), indexes(&c)));
}

/// And the path a deployment actually takes — `SqliteStore::open` on a file — lands on the same
/// schema as the plan applied by hand.
#[test]
fn opening_a_store_applies_the_same_schema() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("m14.db");
    let store = lighttrack_store::SqliteStore::open(&path).expect("open");
    drop(store);
    let opened = Connection::open(&path).expect("reopen");
    let old = legacy_db();
    assert_eq!(tables(&opened), tables(&old));
    for t in tables(&old) {
        assert_eq!(columns(&opened, &t), columns(&old, &t), "table `{t}`");
    }
    assert_eq!(indexes(&opened), indexes(&old));
}
