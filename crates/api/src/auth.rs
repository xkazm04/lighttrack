//! API-key generation, hashing, and request authentication.
//!
//! Keys look like `lt_<prefix>_<secret>`. We store the (non-secret) `prefix` for lookup and a
//! salted SHA-256 of the full key as `key_hash` = `"<salt>:<hex_digest>"`. The raw key is shown
//! to the operator exactly once, at creation.
//!
//! Auth modes:
//!   - `dev`      : relaxed. Requests with no key act as [`Principal::Dev`]; a valid project key is
//!                  still honored. Intended for local development.
//!   - `enforced` : every protected route needs either the admin key (=> [`Principal::Admin`]) or a
//!                  valid project key (=> [`Principal::Project`]); otherwise 401.

use sha2::{Digest, Sha256};

use lighttrack_core::new_id;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Dev,
    Enforced,
}

impl AuthMode {
    pub fn from_env(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "enforced" | "enforce" | "prod" => AuthMode::Enforced,
            _ => AuthMode::Dev,
        }
    }
}

/// The authenticated identity behind a request.
#[derive(Debug, Clone)]
pub enum Principal {
    /// No/relaxed auth (dev mode, no key presented).
    Dev,
    /// The admin key was presented.
    Admin,
    /// A valid project key was presented; carries its project id.
    Project(String),
}

/// A freshly minted key. `full_key` is returned to the caller once and never stored.
pub struct GeneratedKey {
    pub prefix: String,
    pub full_key: String,
    pub key_hash: String,
}

pub(crate) fn sha256_hex(input: &str) -> String {
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn hash_with_salt(salt: &str, full_key: &str) -> String {
    sha256_hex(&format!("{salt}:{full_key}"))
}

/// Build the stored `"<salt>:<digest>"` form for a key.
fn stored_hash(full_key: &str) -> String {
    let salt = new_id().replace('-', "");
    format!("{salt}:{}", hash_with_salt(&salt, full_key))
}

/// Verify a presented key against a stored `"<salt>:<digest>"` hash.
pub fn verify_key(stored: &str, full_key: &str) -> bool {
    match stored.split_once(':') {
        Some((salt, digest)) => hash_with_salt(salt, full_key) == digest,
        None => false,
    }
}

/// Generate a new API key (high-entropy, ~244 bits from two UUIDv4 secrets).
pub fn generate_key() -> GeneratedKey {
    let prefix = new_id().replace('-', "")[..8].to_string();
    let secret = format!(
        "{}{}",
        new_id().replace('-', ""),
        new_id().replace('-', "")
    );
    let full_key = format!("lt_{prefix}_{secret}");
    let key_hash = stored_hash(&full_key);
    GeneratedKey {
        prefix,
        full_key,
        key_hash,
    }
}

/// Extract the `prefix` from a full key string `lt_<prefix>_<secret>`.
pub fn prefix_of(full_key: &str) -> Option<String> {
    let mut parts = full_key.splitn(3, '_');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("lt"), Some(prefix), Some(_secret)) if !prefix.is_empty() => Some(prefix.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_verify_roundtrip() {
        let k = generate_key();
        assert!(k.full_key.starts_with(&format!("lt_{}_", k.prefix)));
        assert_eq!(prefix_of(&k.full_key).as_deref(), Some(k.prefix.as_str()));
        assert!(verify_key(&k.key_hash, &k.full_key));
        assert!(!verify_key(&k.key_hash, "lt_wrong_key"));
    }

    #[test]
    fn rejects_malformed() {
        assert!(prefix_of("nope").is_none());
        assert!(prefix_of("lt__secret").is_none());
        assert!(!verify_key("no-colon", "lt_a_b"));
    }
}
