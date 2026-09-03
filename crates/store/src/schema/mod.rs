//! Schema-as-data: one declarative model, three rendered DDLs, one migration list per dialect.
//!
//! Before M14 the logical schema lived in five hand-kept places — `schema/sqlite/001_init.sql`,
//! `schema/postgres/001_init.sql`, `schema/bigquery/001_init.sql`, `ADDED_COLUMNS` /
//! `ADDED_COLUMNS_LATE` in the SQLite backend, and the per-table `COLS` strings in each backend —
//! each of whose headers claimed to mirror another. They had already drifted. Adding one column was
//! about nine coordinated edits across three crates, and *nothing failed* when one was missed: the
//! column was simply absent on a backend, which reads as "no data".
//!
//! Now [`tables`] is the schema, and everything else is a projection of it:
//!
//! * [`render_sqlite`] / [`render_pg`] / [`render_bq`] produce the three checked-in `.sql` files,
//!   which `crates/store/tests/schema_doc.rs` re-renders and compares (`UPDATE_SCHEMA_SQL=1`
//!   rewrites them), exactly as `parity_doc.rs` does for `docs/PARITY.md`.
//! * [`migrations::plan`] is the ordered statement list a backend applies on open.
//! * `Table::select_list` / `Table::insert_stmt` derive the per-table `COLS` and placeholder
//!   strings the row mappers used to spell out by hand.
//! * [`fingerprint`] hashes the model, and rides in the capability manifest and
//!   `GET /v1/capabilities` so an operator can tell two deployments' schemas apart.

pub mod cols;
pub mod migrations;
pub mod model;
pub mod render;
pub mod render_bq;
pub mod render_doc;
pub mod render_pg;
pub mod render_sqlite;
pub mod tables;

pub use cols::SelectList;
pub use migrations::{plan, Raw};
pub use model::{Column, Dialect, Index, Kind, Table};

/// A short, stable hash of the logical schema.
///
/// Covers what a *reader* of a database can observe: table names, and per column the name, kind,
/// nullability, default and primary-key membership, plus the declared indexes. Deliberately not
/// covered: doc comments (prose is not schema), and which milestone added a column (`added_in`
/// changes the migration path, not the resulting shape — two deployments that reached the same
/// columns by different routes should agree).
///
/// The digest is the same on every backend, because the *model* is: a fingerprint that differed per
/// dialect would answer "which dialect is this", which the manifest's `backend` already answers.
pub fn fingerprint() -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for t in tables::all() {
        h.update(t.name.as_bytes());
        h.update(b"\x1e");
        for c in t.columns {
            h.update(
                format!(
                    "{}:{}:{}:{}:{}\x1f",
                    c.name,
                    c.kind.as_str(),
                    c.nullable,
                    c.default.unwrap_or(""),
                    c.pk
                )
                .as_bytes(),
            );
        }
        h.update(format!("pk={}\x1e", t.primary_key.join(",")).as_bytes());
        for u in t.unique {
            h.update(format!("u={u}\x1e").as_bytes());
        }
        for i in t.indexes {
            let dialects: Vec<&str> = i.dialects.iter().map(|d| d.as_str()).collect();
            h.update(
                format!(
                    "i={}:{}:{}:{}:{}\x1e",
                    i.name,
                    i.columns,
                    i.unique,
                    i.predicate.unwrap_or(""),
                    dialects.join("+")
                )
                .as_bytes(),
            );
        }
    }
    format!("sha256-{}", &hex(&h.finalize())[..16])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fingerprint_is_stable_and_short() {
        let a = fingerprint();
        assert_eq!(a, fingerprint());
        assert!(a.starts_with("sha256-"));
        assert_eq!(a.len(), 23);
    }

    /// The model covers every table the three DDLs used to declare between them — the enumeration
    /// this item exists to do once. A table dropped from `tables::all()` would silently stop being
    /// created, so the count is pinned rather than left to a reviewer's memory.
    #[test]
    fn the_model_declares_every_table() {
        let names: Vec<&str> = tables::all().iter().map(|t| t.name).collect();
        for want in [
            "projects",
            "api_keys",
            "events",
            "limit_rules",
            "scores",
            "benchmarks",
            "rubrics",
            "jobs",
            "prompts",
            "prompt_versions",
            "benchmark_runs",
            "model_prices",
            "datasets",
            "dataset_items",
            "revenue_events",
            "collective_entries",
            "relay_tasks",
            "margin_policies",
            "schedules",
            "devices",
            "alerts",
            "alert_channels",
            "collective_contributions",
            "labels",
            "calibrations",
        ] {
            assert!(names.contains(&want), "{want} is missing from the model");
        }
        assert_eq!(names.len(), 25, "a table was added or removed: {names:?}");
    }
}
