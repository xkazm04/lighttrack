//! The M26 `model_prices` rebuild, proven against a database created from the **old** schema.
//!
//! Every other migration in this backend is an `ADD COLUMN`, which SQLite applies to an existing
//! table without ceremony. This one changes a primary key, which SQLite cannot do — the table has to
//! be created, copied, dropped and renamed. A rebuild is exactly the kind of migration that passes
//! on a developer's fresh database and destroys an operator's populated one, so it is tested the
//! only way that means anything: on a file that predates it.

use rusqlite::Connection;

use lighttrack_core::{new_id, ModelPriceRow};

use super::SqliteStore;
use crate::Store;

/// `model_prices` exactly as it shipped before M26: one overwritten row per model.
const PRE_M26: &str = "CREATE TABLE model_prices (
  provider              TEXT NOT NULL,
  model                 TEXT NOT NULL,
  input_per_mtok        REAL NOT NULL,
  output_per_mtok       REAL NOT NULL,
  cached_input_per_mtok REAL,
  effective_date        TEXT NOT NULL,
  source_url            TEXT,
  PRIMARY KEY (provider, model)
);";

/// Lay down a pre-M26 database file with one stored rate in it, and return its path.
fn pre_m26_fixture(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("pre-m26.db");
    let c = Connection::open(&path).expect("fixture db");
    c.execute_batch(PRE_M26).expect("old schema");
    c.execute(
        "INSERT INTO model_prices (provider, model, input_per_mtok, output_per_mtok, \
         cached_input_per_mtok, effective_date, source_url) \
         VALUES ('legacy','gpt-old',1.5,4.5,0.15,'2026-01-01T00:00:00.000000000Z','http://x')",
        [],
    )
    .expect("legacy row");
    drop(c);
    path
}

fn columns(c: &Connection, table: &str) -> Vec<String> {
    let mut stmt = c
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("table_info");
    stmt.query_map([], |r| r.get::<_, String>(1))
        .expect("cols")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("cols")
}

#[test]
fn opening_a_pre_m26_database_rebuilds_the_price_table_without_losing_a_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = pre_m26_fixture(dir.path());

    let store = SqliteStore::open(&path).expect("open migrates");

    // The shape changed…
    let c = Connection::open(&path).expect("reopen");
    let cols = columns(&c, "model_prices");
    for want in ["effective_from", "verified_at", "note"] {
        assert!(cols.contains(&want.to_string()), "missing column {want}");
    }
    assert!(
        !cols.contains(&"effective_date".to_string()),
        "the old date column was renamed, not left beside its replacement"
    );

    // …the key is now the three-column one, which is what makes the book append-only…
    let pk: Vec<String> = {
        let mut stmt = c.prepare("PRAGMA table_info(model_prices)").expect("info");
        stmt.query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?)))
            .expect("pk")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("pk")
            .into_iter()
            .filter(|(_, k)| *k > 0)
            .map(|(n, _)| n)
            .collect()
    };
    assert_eq!(pk, vec!["provider", "model", "effective_from"]);

    // …and the operator's stored rate survived, dated by the row it already carried.
    let row = store
        .list_prices()
        .expect("list")
        .into_iter()
        .find(|p| p.provider == "legacy")
        .expect("the legacy rate is still in the book");
    assert!((row.input_per_mtok - 1.5).abs() < 1e-9);
    assert_eq!(row.source_url.as_deref(), Some("http://x"));
    assert_eq!(
        row.effective_from.to_rfc3339(),
        "2026-01-01T00:00:00+00:00",
        "the old effective_date became the row's effective_from"
    );
    assert_eq!(
        row.verified_at, None,
        "nobody vouched for a pre-M26 rate; claiming they did would make the staleness \
         warning repeat a lie"
    );

    // The migrated table really is append-only now.
    let mut later = row.clone();
    later.input_per_mtok = 9.0;
    later.effective_from = row.effective_from + chrono::Duration::days(30);
    store.upsert_price(&later).expect("append");
    assert_eq!(
        store
            .list_price_history("legacy", "gpt-old")
            .expect("history")
            .len(),
        2
    );
}

/// Re-opening an already-migrated file must be a no-op, not a second rebuild — a rebuild that ran
/// on every open would drop and recreate the price table on every process start.
#[test]
fn the_rebuild_is_idempotent_across_opens() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = pre_m26_fixture(dir.path());

    let model = format!("m-{}", new_id());
    {
        let store = SqliteStore::open(&path).expect("first open");
        store
            .upsert_price(&ModelPriceRow {
                provider: "legacy".into(),
                model: model.clone(),
                input_per_mtok: 2.0,
                output_per_mtok: 3.0,
                cached_input_per_mtok: None,
                effective_from: chrono::Utc::now(),
                source_url: None,
                verified_at: Some(chrono::Utc::now()),
                note: Some("written after the rebuild".into()),
            })
            .expect("write");
    }

    let store = SqliteStore::open(&path).expect("second open");
    let rows = store.list_prices().expect("list");
    assert_eq!(
        rows.iter().filter(|p| p.model == model).count(),
        1,
        "the second open kept the row the first one wrote"
    );
    assert!(rows.iter().any(|p| p.model == "gpt-old"));
}
