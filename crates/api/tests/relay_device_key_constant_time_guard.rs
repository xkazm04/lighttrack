//! Structural guard for the relay device-key authentication compare (see `relay::ensure_device`).
//!
//! The enrolled device key (`LIGHTTRACK_RELAY_DEVICE_KEY`) authenticates the lease/result endpoints.
//! It must be compared with `auth::secret_eq` (SHA-256 then `subtle::ct_eq`, constant-time and
//! length-hiding) — never a plain `==` on the bearer token, which short-circuits on the first
//! differing byte and leaks the key byte-by-byte to a timing oracle (CWE-208). This guard fails if
//! anyone reintroduces a raw equality compare of the bearer token against the expected key.
//!
//! Corpus class: one-authority / timing-defenses (nonconstant-time-secret-compare).

use std::fs;
use std::path::Path;

// Assembled at runtime so this guard file's own source can't trip the scan.
fn raw_compare_needle() -> String {
    format!("token {} expected", "==")
}

#[test]
fn relay_device_key_uses_constant_time_compare() {
    let src = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/relay.rs"))
        .expect("read relay.rs");

    // The device-key path must route through the constant-time helper.
    assert!(
        src.contains("secret_eq"),
        "relay.rs no longer references auth::secret_eq — the device-key compare must be \
         constant-time (SHA-256 + subtle::ct_eq). A plain `==` leaks the key byte-by-byte."
    );

    // And it must NOT compare the bearer token to the expected key with a raw `==`.
    let needle = raw_compare_needle();
    assert!(
        !src.contains(&needle),
        "relay.rs contains a raw `{needle}` compare — short-circuits on the first differing byte \
         (CWE-208 timing oracle). Use crate::auth::secret_eq(&token, expected) instead."
    );
}
