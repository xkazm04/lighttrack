//! Webhook signatures — the sender's and the receiver's half, in one place.
//!
//! This lives in `core` rather than in the API because the responder *verifies* what the API
//! *signs*. A signature scheme with two implementations is a scheme with two behaviours, and the
//! failure mode is silent: deliveries that verify in a test and are rejected in production.
//!
//! An unsigned POST to an operator-supplied URL is a body anyone can forge: the responder acts on
//! what it receives (it runs Claude, it can open a PR), so "this really came from your LightTrack"
//! has to be checkable. The header follows the shape receivers already know from Stripe:
//!
//! ```text
//! X-LightTrack-Signature: t=1756732800,v1=<hex hmac-sha256 over "t.body">
//! ```
//!
//! The timestamp is inside the signed string, so a captured body cannot be replayed at a later time
//! without invalidating it — a receiver rejects a `t` outside its tolerance.
//!
//! **What the key is.** `AlertChannel::secret_hash` holds `sha256(secret)` as hex — the *derived*
//! signing key. The plaintext secret is minted server-side, shown once on create (the API-key
//! pattern) and never stored, so a database leak does not hand out a secret an operator may have
//! reused elsewhere. A receiver derives the same key from the secret it was shown: `key =
//! sha256(secret)`, then `HMAC-SHA256(key, "t.body")`. `docs/ALERTS.md` states this verbatim.
//!
//! During a rotation a channel carries both the current and previous key and the header carries a
//! `v1=` for each, so a receiver that has not yet picked up the new secret still verifies.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// The header a signed delivery carries.
pub const SIGNATURE_HEADER: &str = "X-LightTrack-Signature";

/// The derived signing key for a plaintext secret: `sha256(secret)`, lowercase hex.
pub fn derive_key(secret: &str) -> String {
    let mut h = Sha256::new();
    h.update(secret.as_bytes());
    hex(&h.finalize())
}

/// `HMAC-SHA256(key, "<t>.<body>")` as lowercase hex.
///
/// `Hmac::new_from_slice` accepts any key length for HMAC, so the `expect` is unreachable — but it
/// is still not an `unwrap` on I/O: the key here is a hex string this process derived.
fn mac(key: &str, t: i64, body: &str) -> Option<String> {
    let mut m = HmacSha256::new_from_slice(key.as_bytes()).ok()?;
    m.update(format!("{t}.{body}").as_bytes());
    Some(hex(&m.finalize().into_bytes()))
}

/// The full header value for `body` at unix time `t`, signed with `key` and, during a rotation,
/// `prev` as well. `None` when the channel has no key — an unsigned channel is a choice an operator
/// made, not an error.
pub fn signature_header(
    key: Option<&str>,
    prev: Option<&str>,
    t: i64,
    body: &str,
) -> Option<String> {
    let mut parts = vec![format!("t={t}")];
    let mut any = false;
    for k in [key, prev].into_iter().flatten() {
        if let Some(v) = mac(k, t, body) {
            parts.push(format!("v1={v}"));
            any = true;
        }
    }
    any.then(|| parts.join(","))
}

/// Verify a received header against `secret` (the plaintext the receiver was shown once), rejecting
/// a timestamp further than `tolerance_secs` from `now`.
///
/// The responder's half of the contract. A stale `t` is refused even when the MAC is right, so a
/// captured delivery cannot be replayed at a webhook that acts on what it receives.
pub fn verify(header: &str, secret: &str, body: &str, now: i64, tolerance_secs: i64) -> bool {
    let mut t = None;
    let mut candidates = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => t = v.parse::<i64>().ok(),
            Some(("v1", v)) => candidates.push(v.to_string()),
            _ => {}
        }
    }
    let Some(t) = t else { return false };
    if (now - t).abs() > tolerance_secs {
        return false;
    }
    let Some(expected) = mac(&derive_key(secret), t, body) else {
        return false;
    };
    candidates.iter().any(|c| constant_time_eq(c, &expected))
}

/// Compare two hex digests without an early return on the first differing byte.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "whsec_test_9f2a";
    const BODY: &str = r#"{"event":"limit_breach","text":"over budget"}"#;

    /// The round trip the whole scheme rests on: what we send verifies with the secret we showed.
    #[test]
    fn a_signed_body_verifies_with_the_secret_it_was_signed_for() {
        let key = derive_key(SECRET);
        let h = signature_header(Some(&key), None, 1_756_732_800, BODY).expect("signed");
        assert!(h.starts_with("t=1756732800,v1="));
        assert!(verify(&h, SECRET, BODY, 1_756_732_800, 300));
    }

    #[test]
    fn a_tampered_body_or_a_wrong_secret_does_not_verify() {
        let key = derive_key(SECRET);
        let h = signature_header(Some(&key), None, 1_756_732_800, BODY).expect("signed");
        assert!(
            !verify(
                &h,
                SECRET,
                r#"{"event":"limit_breach","text":"fine"}"#,
                1_756_732_800,
                300
            ),
            "the body is inside the signed string, so editing it must break the signature"
        );
        assert!(!verify(&h, "whsec_other", BODY, 1_756_732_800, 300));
    }

    /// The timestamp is signed, so a captured delivery cannot be replayed an hour later.
    #[test]
    fn a_stale_timestamp_is_refused_even_though_the_mac_is_right() {
        let key = derive_key(SECRET);
        let t = 1_756_732_800;
        let h = signature_header(Some(&key), None, t, BODY).expect("signed");
        assert!(verify(&h, SECRET, BODY, t + 120, 300));
        assert!(!verify(&h, SECRET, BODY, t + 3600, 300));
    }

    /// A rotation must not drop deliveries: both keys ride on the wire, so a receiver on either
    /// side of the change verifies.
    #[test]
    fn both_sides_of_a_rotation_verify() {
        let old = "whsec_old";
        let new = "whsec_new";
        let h = signature_header(
            Some(&derive_key(new)),
            Some(&derive_key(old)),
            1_756_732_800,
            BODY,
        )
        .expect("signed");
        assert_eq!(h.matches("v1=").count(), 2);
        assert!(verify(&h, new, BODY, 1_756_732_800, 300));
        assert!(verify(&h, old, BODY, 1_756_732_800, 300));
    }

    /// A channel with no key sends no header, rather than a header signed with nothing.
    #[test]
    fn an_unkeyed_channel_produces_no_header() {
        assert!(signature_header(None, None, 1, BODY).is_none());
    }

    #[test]
    fn a_malformed_header_is_refused_rather_than_parsed_loosely() {
        assert!(!verify("garbage", SECRET, BODY, 1, 300));
        assert!(!verify("v1=deadbeef", SECRET, BODY, 1, 300), "no timestamp");
    }

    /// The derived key is the receiver's half of the contract — if this changed, every configured
    /// receiver would start rejecting.
    #[test]
    fn the_derived_key_is_plain_sha256_of_the_secret() {
        assert_eq!(derive_key("").len(), 64);
        assert_eq!(
            derive_key("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
