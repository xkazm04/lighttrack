//! `schema/{sqlite,postgres,bigquery}/001_init.sql` are generated, and this test is what keeps them
//! true.
//!
//! Same shape as `parity_doc.rs`: render each file from the declarative model in
//! `crates/store/src/schema/tables/` and compare it to the file on disk. Adding a column changes
//! the model, which changes the render, which fails here until the files are regenerated — so the
//! three DDLs cannot drift from each other or from the code the way three hand-kept copies did.
//!
//! Set `UPDATE_SCHEMA_SQL=1` to rewrite the files instead of failing.
//!
//! No database is opened: every renderer is a pure function of the model.

use std::fs;
use std::path::PathBuf;

use lighttrack_store::schema::{render_bq, render_pg, render_sqlite};

fn path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn check(rel: &str, rendered: String) {
    let p = path(rel);
    if std::env::var("UPDATE_SCHEMA_SQL").is_ok_and(|v| !v.is_empty()) {
        fs::write(&p, &rendered).unwrap_or_else(|e| panic!("write {rel}: {e}"));
        return;
    }
    let on_disk = fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "{rel} is missing or unreadable ({e}) — regenerate it with \
             `UPDATE_SCHEMA_SQL=1 cargo test -p lighttrack-store --test schema_doc`"
        )
    });
    assert_eq!(
        on_disk.replace("\r\n", "\n"),
        rendered,
        "{rel} is stale — the schema model changed. Regenerate with \
         `UPDATE_SCHEMA_SQL=1 cargo test -p lighttrack-store --test schema_doc`"
    );
}

#[test]
fn sqlite_ddl_matches_the_model() {
    check("schema/sqlite/001_init.sql", render_sqlite::ddl_file());
}

#[test]
fn postgres_ddl_matches_the_model() {
    check("schema/postgres/001_init.sql", render_pg::ddl_file());
}

#[test]
fn bigquery_ddl_matches_the_model() {
    check("schema/bigquery/001_init.sql", render_bq::ddl_file());
}

/// `docs/DATA_MODEL.md` keeps its prose and gains a generated index, so the document can no longer
/// silently describe eight of twenty-five tables with nothing saying so.
#[test]
fn data_model_index_matches_the_model() {
    use lighttrack_store::schema::render_doc;
    let p = path("docs/DATA_MODEL.md");
    let on_disk = fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("read docs/DATA_MODEL.md: {e}"))
        .replace("\r\n", "\n");
    let rendered = render_doc::splice(&on_disk);
    if std::env::var("UPDATE_SCHEMA_SQL").is_ok_and(|v| !v.is_empty()) {
        fs::write(&p, &rendered).expect("write docs/DATA_MODEL.md");
        return;
    }
    assert_eq!(
        on_disk, rendered,
        "docs/DATA_MODEL.md's generated table index is stale — regenerate with \
         `UPDATE_SCHEMA_SQL=1 cargo test -p lighttrack-store --test schema_doc`"
    );
}

/// The drift the item was opened on: BigQuery had a third of the tables and no way to notice.
/// All three dialects now declare the same table set — the counts are equal by construction, and
/// this is the assertion that says so out loud.
#[test]
fn the_three_dialects_declare_the_same_tables() {
    use lighttrack_store::schema::tables;
    let (sqlite, pg, bq) = (
        render_sqlite::ddl_file(),
        render_pg::ddl_file(),
        render_bq::ddl_file(),
    );
    for t in tables::all() {
        assert!(sqlite.contains(&format!("CREATE TABLE IF NOT EXISTS {} (", t.name)));
        assert!(pg.contains(&format!("CREATE TABLE IF NOT EXISTS {} (", t.name)));
        assert!(bq.contains(&format!("`${{DATASET}}.{}`", t.name)));
    }
}
