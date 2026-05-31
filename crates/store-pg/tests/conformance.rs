//! Postgres conformance — runs only when `LIGHTTRACK_TEST_DATABASE_URL` is set (point it at a
//! throwaway/empty database; the suite writes rows). Skips (passes as a no-op) otherwise, so CI
//! without a Postgres stays green.
//!
//!   LIGHTTRACK_TEST_DATABASE_URL=postgres://postgres:pw@127.0.0.1:5433/lighttrack \
//!     cargo test -p lighttrack-store-pg

use lighttrack_store::conformance;
use lighttrack_store_pg::PgStore;

#[test]
fn pg_conformance() {
    let url = match std::env::var("LIGHTTRACK_TEST_DATABASE_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("skipping pg_conformance: set LIGHTTRACK_TEST_DATABASE_URL=postgres://… to run");
            return;
        }
    };
    let store = PgStore::connect(&url).expect("connect postgres");
    conformance::run(&store).expect("postgres conformance");
}
