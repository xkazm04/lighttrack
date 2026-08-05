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
use subtle::ConstantTimeEq;

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

/// Say out loud, at boot, that this instance is not authenticating anything.
///
/// `dev` stays the default on purpose — a self-hosted tool has to work on the first run with no
/// configuration at all, and any unset/typo'd `LIGHTTRACK_AUTH_MODE` lands here. What is *not*
/// acceptable is doing it silently: the banner line reads `auth=Dev` in the middle of a long
/// diagnostics string, which is easy to run past on the way to production. So this is a block, on
/// stderr, next to the other startup diagnostics.
pub(crate) fn warn_if_unenforced(mode: AuthMode) {
    if mode == AuthMode::Enforced {
        return;
    }
    eprintln!(
        "
!!!!! WARNING: AUTHENTICATION IS NOT ENFORCED (LIGHTTRACK_AUTH_MODE=dev) !!!!!
      Every request is accepted. A request with NO bearer token, and a request with ANY
      unrecognized bearer token, both authenticate as an admin-equivalent principal: they can
      read, write and administer every project on this instance.
      This is the zero-config default so a first run works out of the box, and it is what an
      unset or misspelled LIGHTTRACK_AUTH_MODE falls back to.
      Before this instance is reachable by anything but localhost, set:
          LIGHTTRACK_AUTH_MODE=enforced
      and give it credentials (LIGHTTRACK_ADMIN_KEY=<secret>, plus per-project API keys via
      POST /v1/projects/:id/keys).
"
    );
}

/// Constant-time comparison of two secrets.
///
/// Both sides are SHA-256'd to a fixed 32 bytes *before* the compare, which buys two things a plain
/// `==` does not:
///
/// 1. **No early exit on the first differing byte.** `==` on `str`/`[u8]` short-circuits, so its
///    running time is a function of how many leading bytes a guess got right — the byte-at-a-time
///    credential oracle (CWE-208).
/// 2. **No length leak.** Any slice comparison (including `subtle`'s own `[u8]::ct_eq`) has to
///    return early when the lengths differ, which tells an attacker how long the real secret is.
///    Digesting first makes both operands fixed-width, so the length of either never reaches the
///    comparison at all.
///
/// `subtle` rather than a hand-rolled XOR fold: it is already compiled into this crate's dependency
/// graph (`sha2` -> `digest` -> `subtle`), so it adds no build cost or supply-chain surface, and it
/// carries the optimization barriers that keep a compiler from reintroducing the branch a
/// hand-written loop only *looks* free of.
pub(crate) fn secret_eq(presented: &str, expected: &str) -> bool {
    let a = Sha256::digest(presented.as_bytes());
    let b = Sha256::digest(expected.as_bytes());
    a.as_slice().ct_eq(b.as_slice()).into()
}

/// The authenticated identity behind a request.
#[derive(Debug, Clone)]
pub enum Principal {
    /// No/relaxed auth (dev mode, no key presented).
    Dev,
    /// The admin key was presented.
    Admin,
    /// A valid project key was presented; carries its project id **and the key's row id**.
    ///
    /// `key_id` is the opaque `api_keys.id` — never the presented token, its prefix, or any hash of
    /// it. Ingest stamps it onto the event so a budget can be scoped to one key and a breach can name
    /// which key drove it; nothing derived from the secret ever leaves this function.
    Project { project_id: String, key_id: String },
}

impl Principal {
    /// The id of the API key behind this request, when one was presented. `None` for admin/dev
    /// principals: those are not keys in the `api_keys` table, so there is nothing honest to
    /// attribute their traffic to — it lands in the "unattributed" bucket rather than borrowing
    /// someone else's identity.
    pub(crate) fn key_id(&self) -> Option<&str> {
        match self {
            Principal::Project { key_id, .. } => Some(key_id),
            Principal::Admin | Principal::Dev => None,
        }
    }
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
///
/// The compare is constant-time ([`secret_eq`]). Lower severity than the admin-key compare — both
/// operands are post-hash, so a timing oracle leaks progress toward a *digest* the attacker still
/// cannot invert — but the same anti-pattern, and it costs nothing to close.
pub fn verify_key(stored: &str, full_key: &str) -> bool {
    match stored.split_once(':') {
        Some((salt, digest)) => secret_eq(&hash_with_salt(salt, full_key), digest),
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
        // A truncated/garbage stored digest is a plain mismatch, not a panic — the compare must
        // tolerate operands of unequal length (see `secret_eq`).
        assert!(!verify_key("salt:deadbeef", "lt_a_b"));
        assert!(!verify_key("salt:", "lt_a_b"));
    }

    #[test]
    fn secret_eq_decides_equality_exactly_as_plain_comparison_did() {
        // The property [`secret_eq`] actually buys is *timing-invariance*, and that is deliberately
        // NOT asserted here: a timing measurement is unobservable from a unit test and would be
        // flaky by construction on a shared runner. What is testable — and what a future refactor
        // could silently break — is that it still decides correctness the same way `==` did, for
        // every shape of input including the unequal-length ones a length check would short-circuit.
        assert!(secret_eq("s3cret", "s3cret"));
        assert!(secret_eq("", ""));
        assert!(!secret_eq("s3cret", "s3crEt")); // same length, one byte apart
        assert!(!secret_eq("s3cret", "s3cre")); // expected is a prefix of presented
        assert!(!secret_eq("s3cret", "s3cretX")); // presented is a prefix of expected
        assert!(!secret_eq("s3cret", "")); // length 6 vs 0
        assert!(!secret_eq("", "s3cret"));
        assert!(!secret_eq("abc", "cba")); // same bytes, different order
        assert!(!secret_eq("s3cret", "S3CRET")); // case is significant
        // Long, realistic key material: equal compares true, a single flipped last byte false.
        let k = generate_key().full_key;
        let mut flipped = k.clone();
        flipped.pop();
        flipped.push('!');
        assert!(secret_eq(&k, &k));
        assert!(!secret_eq(&k, &flipped));
    }
}
