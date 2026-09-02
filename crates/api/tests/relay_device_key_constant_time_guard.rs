//! Structural guard for the relay device-key authentication compare (see
//! `relay_devices::ensure_device`).
//!
//! The deprecated shared device key (`LIGHTTRACK_RELAY_DEVICE_KEY`) still authenticates the
//! lease/result endpoints. It must be compared with `auth::secret_eq` (SHA-256 then
//! `subtle::ct_eq`, constant-time and length-hiding) — never a plain `==` on the bearer token,
//! which short-circuits on the first differing byte and leaks the key byte-by-byte to a timing
//! oracle (CWE-208). This guard fails if anyone reintroduces a raw equality compare of the bearer
//! token against the expected key.
//!
//! The M18 *enrolled* key (`ltd_…`) is compared through `auth::verify_key`, which is constant-time
//! for the same reason and is pinned by `auth.rs`'s own tests; the guard here is about the one
//! compare that takes a raw operator secret.
//!
//! Corpus class: one-authority / timing-defenses (nonconstant-time-secret-compare).

use std::fs;
use std::path::Path;

// Assembled at runtime so this guard file's own source can't trip the scan.
fn raw_compare_needle() -> String {
    format!("token {} expected", "==")
}

/// Where the device guard lives. M18 moved it out of `relay.rs` into `relay_devices.rs`; both are
/// scanned so the guard survives the move without going quietly green on a file that no longer
/// holds the compare — a guard that passes because its subject moved away is worse than no guard.
const GUARDED: &[&str] = &["src/relay_devices.rs", "src/relay.rs"];

#[test]
fn relay_device_key_uses_constant_time_compare() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = Vec::new();
    for rel in GUARDED {
        let path = root.join(rel);
        if path.exists() {
            sources.push((
                *rel,
                fs::read_to_string(&path).expect("read guarded source"),
            ));
        }
    }
    assert!(
        !sources.is_empty(),
        "none of {GUARDED:?} exists — the device guard moved again; point this test at its new home"
    );

    // The device-key path must route through the constant-time helper, wherever it now lives.
    assert!(
        sources.iter().any(|(_, src)| src.contains("secret_eq")),
        "none of {GUARDED:?} references auth::secret_eq — the shared device-key compare must be \
         constant-time (SHA-256 + subtle::ct_eq). A plain `==` leaks the key byte-by-byte."
    );

    // And none of them may compare the bearer token to the expected key with a raw `==`.
    let needle = raw_compare_needle();
    for (rel, src) in &sources {
        assert!(
            !src.contains(&needle),
            "{rel} contains a raw `{needle}` compare — short-circuits on the first differing byte \
             (CWE-208 timing oracle). Use crate::auth::secret_eq(&token, expected) instead."
        );
    }
}
