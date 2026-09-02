//! API-key generation, hashing, and request authentication.
//!
//! Keys look like `lt_<prefix>_<secret>`. We store the (non-secret) `prefix` for lookup and a
//! salted SHA-256 of the full key as `key_hash` = `"<salt>:<hex_digest>"`. The raw key is shown
//! to the operator exactly once, at creation.
//!
//! Auth modes:
//! - `dev`: relaxed. Requests with no key act as [`Principal::Dev`]; a valid project key is still
//!   honored. Intended for local development.
//! - `enforced`: every protected route needs either the admin key (=> [`Principal::Admin`]) or a
//!   valid project key (=> [`Principal::Project`]); otherwise 401.

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use lighttrack_core::{new_id, Scope};
use lighttrack_store::Scope as TenantScope;

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
    /// A valid project key was presented; carries its project id, the key's row id **and the
    /// capabilities that key was minted with**.
    ///
    /// `key_id` is the opaque `api_keys.id` — never the presented token, its prefix, or any hash of
    /// it. Ingest stamps it onto the event so a budget can be scoped to one key and a breach can name
    /// which key drove it; nothing derived from the secret ever leaves this function.
    ///
    /// `scopes` travels on the principal rather than being re-read per check: the key row was
    /// already loaded to verify the secret, and a second read per authorization would put a store
    /// round-trip on every door.
    Project {
        project_id: String,
        key_id: String,
        scopes: Vec<Scope>,
    },
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

    /// The tenant scope every store read this request makes must carry (M17).
    ///
    /// A project key reads exactly its own rows, so a foreign id is simply not found — the 404 D13
    /// established for traces, now applied to the whole trait. Admin and dev are the operator: every
    /// project, plus the project-less rows (background jobs, global alert channels) no tenant owns.
    ///
    /// This is the only place the mapping is made. A handler that reaches past it and passes
    /// `TenantScope::Operator` on a project request has re-opened the hole — which is why the post-hoc
    /// `forbidden(...)` comparisons this replaces are deleted rather than kept as a second belt: a
    /// 403 on a foreign id is itself the existence oracle.
    pub(crate) fn scope(&self) -> TenantScope<'_> {
        match self {
            Principal::Project { project_id, .. } => TenantScope::Project(project_id),
            Principal::Admin | Principal::Dev => TenantScope::Operator,
        }
    }

    /// [`Principal::scope`] as an owned value, for the `'static` closures `spawn_db` hands to the
    /// blocking pool. Rebuild the borrowed form inside the closure with `.as_deref().into()`.
    pub(crate) fn scope_owned(&self) -> Option<String> {
        self.scope().project().map(str::to_string)
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

/// Scheme marker on a project API key.
const KEY_SCHEME: &str = "lt";

/// Scheme marker on a **relay device** key (M18). Distinct from [`KEY_SCHEME`] so the two can never
/// be confused for one another: a device key that parsed as a project key would be looked up in the
/// wrong table, and the miss would read as "bad credential" rather than "wrong kind of credential".
/// The scheme is also what lets the device guard tell "this is a device key that failed" from "this
/// is not a device key at all, try the legacy shared secret".
pub const DEVICE_KEY_SCHEME: &str = "ltd";

/// Generate a new API key (high-entropy, ~244 bits from two UUIDv4 secrets).
pub fn generate_key() -> GeneratedKey {
    generate_scheme(KEY_SCHEME)
}

/// Generate a new **device** key, `ltd_<prefix>_<secret>` — same entropy, same salted-digest
/// storage, different scheme (see [`DEVICE_KEY_SCHEME`]). Shown to the operator once, at enrolment.
pub fn generate_device_key() -> GeneratedKey {
    generate_scheme(DEVICE_KEY_SCHEME)
}

fn generate_scheme(scheme: &str) -> GeneratedKey {
    let prefix = new_id().replace('-', "")[..8].to_string();
    let secret = format!("{}{}", new_id().replace('-', ""), new_id().replace('-', ""));
    let full_key = format!("{scheme}_{prefix}_{secret}");
    let key_hash = stored_hash(&full_key);
    GeneratedKey {
        prefix,
        full_key,
        key_hash,
    }
}

/// Extract the `prefix` from a full key string `lt_<prefix>_<secret>`.
pub fn prefix_of(full_key: &str) -> Option<String> {
    scheme_prefix_of(KEY_SCHEME, full_key)
}

/// Extract the `prefix` from a device key `ltd_<prefix>_<secret>`, or `None` when the presented
/// token is not a device key at all.
pub fn device_prefix_of(full_key: &str) -> Option<String> {
    scheme_prefix_of(DEVICE_KEY_SCHEME, full_key)
}

fn scheme_prefix_of(scheme: &str, full_key: &str) -> Option<String> {
    let mut parts = full_key.splitn(3, '_');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(s), Some(prefix), Some(_secret)) if s == scheme && !prefix.is_empty() => {
            Some(prefix.to_string())
        }
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
    fn a_device_key_and_a_project_key_are_never_mistaken_for_one_another() {
        // The two schemes share the hashing and the entropy but not the namespace. A device key
        // that parsed as a project key would be looked up in `api_keys`, and the miss would read as
        // "bad credential" instead of "wrong kind of credential".
        let d = generate_device_key();
        assert!(d.full_key.starts_with("ltd_"));
        assert_eq!(
            device_prefix_of(&d.full_key).as_deref(),
            Some(d.prefix.as_str())
        );
        assert!(verify_key(&d.key_hash, &d.full_key));
        assert!(
            prefix_of(&d.full_key).is_none(),
            "a device key is not a project key"
        );

        let k = generate_key();
        assert!(
            device_prefix_of(&k.full_key).is_none(),
            "a project key is not a device key — and this `None` is what makes the device guard \
             fall through to the legacy shared secret rather than 401 on a valid admin key"
        );
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
