//! `docs/PARITY.md` is generated, and this test is what keeps it true.
//!
//! Same shape as `ts_format_guard.rs`: render the document from the three backends' manifests and
//! compare it to the file on disk. A backend that ports a surface (or quietly loses one) changes its
//! `SURFACES` const, which changes the render, which fails here until the doc is regenerated — so
//! the published parity matrix cannot drift from the code the way a hand-written table does.
//!
//! Set `UPDATE_PARITY_DOC=1` to rewrite the file instead of failing.
//!
//! No database is opened: each backend's manifest is a pure associated function of its type.

use std::fs;
use std::path::PathBuf;

use lighttrack_store::capabilities::parity_doc;
use lighttrack_store::SqliteStore;
use lighttrack_store_firestore::FirestoreStore;
use lighttrack_store_pg::PgStore;

fn doc_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/PARITY.md")
}

#[test]
fn parity_doc_matches_the_backends_manifests() {
    let rendered = parity_doc(&[
        SqliteStore::manifest(),
        PgStore::manifest(),
        FirestoreStore::manifest(),
    ]);
    let path = doc_path();

    if std::env::var("UPDATE_PARITY_DOC").is_ok_and(|v| !v.is_empty()) {
        fs::write(&path, &rendered).expect("write docs/PARITY.md");
        return;
    }

    let on_disk = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "docs/PARITY.md is missing or unreadable ({e}) — regenerate it with \
             `UPDATE_PARITY_DOC=1 cargo test -p lighttrack-store --test parity_doc`"
        )
    });
    // Normalise line endings: the file is checked out with the platform's, the render emits `\n`.
    assert_eq!(
        on_disk.replace("\r\n", "\n"),
        rendered,
        "docs/PARITY.md is stale — a backend's declared surfaces changed. Regenerate with \
         `UPDATE_PARITY_DOC=1 cargo test -p lighttrack-store --test parity_doc`"
    );
}

/// The document is only worth publishing if it reports the gaps that actually exist today. This
/// pins the two that drive real decisions, so a manifest edited to *hide* a gap fails here as well
/// as in the diff.
#[test]
fn the_matrix_reports_the_gaps_that_exist() {
    use lighttrack_store::Surface;

    let sqlite = SqliteStore::manifest();
    assert_eq!(
        sqlite.missing(),
        Vec::new(),
        "SQLite is the reference backend: it implements every surface"
    );

    let pg = PgStore::manifest();
    assert!(pg.atomic_admission, "Postgres enforces caps atomically");
    assert!(pg.has(Surface::Traces) && pg.has(Surface::ProjectAdmin));
    assert!(
        !pg.has(Surface::Prompts),
        "the registry is not ported to PG"
    );

    let fs = FirestoreStore::manifest();
    assert!(
        !fs.atomic_admission,
        "Firestore admission is check-then-act — its caps are advisory"
    );
    assert!(
        !fs.has(Surface::Traces),
        "Firestore has no server-side grouping by trace_id"
    );
    assert!(fs.has(Surface::Prompts));
}
