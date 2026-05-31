//! SQLite conformance — runs in CI always (in-memory, no external infra).

use lighttrack_store::{conformance, SqliteStore};

#[test]
fn sqlite_conformance() {
    let store = SqliteStore::open_in_memory().expect("open in-memory sqlite");
    conformance::run(&store).expect("sqlite conformance");
}
