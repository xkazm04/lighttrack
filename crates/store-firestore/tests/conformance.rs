//! Firestore conformance — runs only when `LIGHTTRACK_TEST_FIRESTORE` is set (e.g.
//! `firestore://demo`, with `FIRESTORE_EMULATOR_HOST` pointing at a running emulator). Skips
//! (passes as a no-op) otherwise, so CI without an emulator stays green.
//!
//!   FIRESTORE_EMULATOR_HOST=127.0.0.1:8080 LIGHTTRACK_TEST_FIRESTORE=firestore://demo \
//!     cargo test -p lighttrack-store-firestore

use lighttrack_store::conformance;
use lighttrack_store_firestore::FirestoreStore;

#[test]
fn firestore_conformance() {
    let url = match std::env::var("LIGHTTRACK_TEST_FIRESTORE") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!(
                "skipping firestore_conformance: set LIGHTTRACK_TEST_FIRESTORE=firestore://<project> \
                 (and FIRESTORE_EMULATOR_HOST) to run"
            );
            return;
        }
    };
    let store = FirestoreStore::connect(&url).expect("connect firestore");
    conformance::run(&store).expect("firestore conformance");
}
