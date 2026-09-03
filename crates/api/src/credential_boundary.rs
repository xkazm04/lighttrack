//! The credential boundary: what an operator surface may emit, and whether this process may boot.
//!
//! Two mechanisms, deliberately nothing else.
//!
//! **The emission allowlist.** [`render_operator_record`] renders a diagnostic record *this process
//! constructed itself*. That is the discriminator against [`crate::redact`], and half the value of
//! this module is stating it: `redact` (and `lighttrack-anon`) own free text a *caller* sent, whose
//! shape is not ours to enumerate, so a keyed denylist plus a pattern sweep is the only instrument
//! available there — `docs/DECISIONS.md` D14 records what that costs. Here the record is assembled
//! out of our own request context, in one function, at one moment. Under those conditions the
//! denylist is strictly weaker: the credential field next quarter's integration adds, under a name
//! nobody in this file has heard, would be published by default and the first person to learn about
//! it is whoever is reading the stream. So the named keys survive and nothing else does. It is the
//! generalization of `redact`'s `METADATA_PASSTHROUGH`, applied at the *outbound* seam.
//!
//! **The boot gate.** [`check_boot`] decides whether an instance that authenticates nobody may start
//! at all. `crates/agent/src/config.rs:57-70` is the in-tree precedent — it resolves every device-key
//! env var at load "so a missing secret fails at startup, not on the first lease". The API server is
//! the binary that holds every provider credential it benchmarks with, and it was the one without
//! that check: it printed [`crate::auth::warn_if_unenforced`] and then served.
//!
//! What this module does **not** own: principal resolution, the constant-time compare and the
//! failed-auth throttle stay in `guards`/`auth`; this decides only whether the process may boot into
//! the posture those produce. `Principal::Dev` itself is untouched — the zero-config first run still
//! works, it just stops being reachable over a network without someone naming the trade.

use axum::http::HeaderMap;

use crate::auth::AuthMode;

/// What replaces a value that may not be rendered. The marker sits at the original key rather than
/// removing it, because half of what an operator reads a stream for is *shape* — whether a
/// credential was attached at all is exactly the question a dropped key cannot answer.
pub(crate) const REDACTED: &str = "[REDACTED]";

/// The keys an operator diagnostic may carry verbatim. Short on purpose: a surviving-key list that
/// grows past what a reviewer can hold in their head is a denylist wearing the other one's clothes.
/// When it wants to grow, the honest question is usually whether the stream needs a new *view*.
const OPERATOR_FIELDS: [&str; 6] = [
    "provider", // which configured integration — a name from our own config, never a secret
    "project",  // the project id, already public in every event response
    "route",    // our own path template
    "status",   // our own outcome vocabulary
    "reason",   // why we refused — the product, for this audience (see the API's error vocabulary)
    "duration_ms",
];

/// Cap on one rendered value. An operator surface that relays an unbounded body is a memory
/// amplifier during the incident it exists for.
const MAX_VALUE_LEN: usize = 256;

/// Render one operator diagnostic record: allowlisted fields, then every request header by name with
/// its value replaced.
///
/// **Headers are a category, not a list.** Every value is replaced unconditionally, without anyone
/// enumerating which are safe. Header names are not ours — proxies, runtimes and client libraries
/// add them, and the header that carries authorization is spelled differently by every party that
/// has ever forwarded a request (`authorization`, `x-api-key`, `webhook-signature`, the next one).
/// There is no version of that list worth maintaining.
///
/// A field that is not on [`OPERATOR_FIELDS`] is dropped rather than marked, and that asymmetry with
/// headers is deliberate: a header *name* is protocol vocabulary, while an unanticipated field name
/// is shaped by whoever added it and can itself be the disclosure (`stripe_live_key`, a customer id
/// used as a key).
pub(crate) fn render_operator_record(fields: &[(&str, String)], headers: &HeaderMap) -> String {
    let mut out: Vec<String> = Vec::new();
    for (k, v) in fields {
        if OPERATOR_FIELDS.contains(k) {
            out.push(format!("{k}={}", cap(v)));
        }
    }
    for name in headers.keys() {
        out.push(format!("header.{name}={REDACTED}"));
    }
    out.join(" ")
}

/// Truncate on a char boundary and say so. `&s[..n]` would panic mid-codepoint on any non-ASCII
/// value, which is a poor way for a diagnostic path to fail.
fn cap(v: &str) -> String {
    match v.char_indices().nth(MAX_VALUE_LEN) {
        None => v.to_string(),
        Some((i, _)) => format!("{}[truncated]", &v[..i]),
    }
}

/// The env var that opts an instance out of the boot gate, and the exact sentence it must hold.
///
/// The value is a sentence rather than `1` so it cannot be set by accident, cannot be inherited from
/// a half-copied compose file, and reads as a decision in a diff. If this ever appears in `deploy/`
/// or in the README quickstart, this change has failed and should be reverted rather than tuned.
pub(crate) const OPT_OUT_ENV: &str = "LIGHTTRACK_ALLOW_UNAUTHENTICATED";
pub(crate) const OPT_OUT_PHRASE: &str = "yes, anyone who can reach this port is an admin";

/// May this process start? Called from `main` before the router is built and long before a port is
/// open — a guard that raised inside a handler would leave the process up, the port bound and the
/// health check green, and the misconfiguration discovered by whoever called a route first.
///
/// Three states, one of which is *stop*:
///
/// 1. `enforced` — every route needs a real credential. Starts.
/// 2. Unenforced, and someone wrote the sentence. Starts.
/// 3. Neither — refuse, naming both remedies in one message. A refusal that does not say how to
///    satisfy it gets satisfied by deleting the check.
///
/// `admin_key` does not make state 3 into state 1: under `AuthMode::Dev` a request with no token and
/// a request with any unrecognized token both resolve to `Principal::Dev`, which `ensure_can_admin`
/// treats as admin-equivalent, so the key gates nothing. It is read anyway — by *value*, not by
/// presence, since an exported-but-empty var satisfies every "is it configured" test and
/// authenticates no one — because "the key is set" is the belief this refusal most often has to
/// correct.
pub(crate) fn check_boot(
    mode: AuthMode,
    admin_key: Option<&str>,
    opt_out: Option<&str>,
) -> anyhow::Result<()> {
    if mode == AuthMode::Enforced {
        return Ok(());
    }
    if opt_out.map(str::trim) == Some(OPT_OUT_PHRASE) {
        return Ok(());
    }
    let key_note = if admin_key.map(str::trim).is_some_and(|s| !s.is_empty()) {
        " LIGHTTRACK_ADMIN_KEY is set, but it gates nothing while the mode is not `enforced`."
    } else {
        ""
    };
    anyhow::bail!(
        "refusing to start: LIGHTTRACK_AUTH_MODE is not `enforced`, so every request — including \
         one carrying no bearer token at all — is served as an admin-equivalent principal, and this \
         process holds every provider credential it benchmarks with.{key_note} Choose one:\n  \
         (a) set LIGHTTRACK_AUTH_MODE=enforced and LIGHTTRACK_ADMIN_KEY=<secret>, then mint \
         per-project keys (POST /v1/projects, then POST /v1/projects/:id/keys); or\n  \
         (b) if this instance is genuinely reachable only from this machine, say so:\n      \
         {OPT_OUT_ENV}=\"{OPT_OUT_PHRASE}\""
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn a_credential_bearing_header_and_an_unknown_field_both_fail_closed() {
        // The two shapes that matter: a header nobody enumerated as sensitive, and a field a future
        // contributor added without reading this file.
        let rendered = render_operator_record(
            &[
                ("provider", "polar".to_string()),
                ("reason", "signature mismatch".to_string()),
                // Not on the allowlist. This is the whole property: it is redacted by *default*,
                // not because anyone anticipated a field with this name.
                ("stripe_live_key", "sk_live_51NxSECRET".to_string()),
            ],
            &headers(&[
                ("authorization", "Bearer lt_abcd_TOPSECRET"),
                ("webhook-signature", "v1,whsec_DEADBEEF"),
                ("content-type", "application/json"),
            ]),
        );

        // What an operator still gets: the shape of the request and why we refused.
        assert!(rendered.contains("provider=polar"), "{rendered}");
        assert!(rendered.contains("reason=signature mismatch"), "{rendered}");
        // Every header is present by name — "was a credential attached on this call" stays
        // answerable — and every value is gone, including the one nobody would have listed.
        assert!(
            rendered.contains(&format!("header.authorization={REDACTED}")),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!("header.webhook-signature={REDACTED}")),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!("header.content-type={REDACTED}")),
            "{rendered}"
        );
        // The unanticipated field is gone, name and all.
        assert!(!rendered.contains("stripe_live_key"), "{rendered}");

        // The assertion that would catch a regression in any of the above at once: no secret this
        // record was built from survives anywhere in the emitted bytes.
        for secret in ["TOPSECRET", "whsec_DEADBEEF", "sk_live_51NxSECRET"] {
            assert!(!rendered.contains(secret), "{secret} survived: {rendered}");
        }
    }

    #[test]
    fn long_values_are_capped_on_a_char_boundary() {
        let long = "é".repeat(MAX_VALUE_LEN + 50);
        let rendered = render_operator_record(&[("reason", long)], &HeaderMap::new());
        assert!(rendered.ends_with("[truncated]"), "{rendered}");
    }

    #[test]
    fn an_unenforced_instance_with_no_admin_key_refuses_to_boot() {
        // The measurable: with LIGHTTRACK_AUTH_MODE and LIGHTTRACK_ADMIN_KEY both unset —
        // `AuthMode::from_env` maps unset and misspelled alike to `Dev` — the process stops.
        let err = check_boot(AuthMode::from_env(""), None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to start"), "{err}");
        // A refusal that does not name its remedies gets satisfied by deleting the check.
        assert!(err.contains("LIGHTTRACK_AUTH_MODE=enforced"), "{err}");
        assert!(err.contains(OPT_OUT_ENV), "{err}");
        assert!(err.contains(OPT_OUT_PHRASE), "{err}");

        // An admin key does not buy a boot in dev mode — it gates nothing there — but it does change
        // the diagnosis, because "but I set the key" is the belief this message has to correct.
        let with_key = check_boot(AuthMode::Dev, Some("s3cret"), None)
            .unwrap_err()
            .to_string();
        assert!(with_key.contains("gates nothing"), "{with_key}");
        // Read the value, not the presence: `KEY=` in a compose file is not a credential.
        let blank = check_boot(AuthMode::Dev, Some("  "), None)
            .unwrap_err()
            .to_string();
        assert!(!blank.contains("gates nothing"), "{blank}");
    }

    #[test]
    fn enforced_mode_and_the_named_opt_out_are_the_only_ways_through() {
        assert!(check_boot(AuthMode::Enforced, Some("s3cret"), None).is_ok());
        // Enforced with no admin key is a bootstrap dead end, not an open door: every route still
        // 401s without a valid key. Closed is allowed to be inconvenient.
        assert!(check_boot(AuthMode::Enforced, None, None).is_ok());
        assert!(check_boot(AuthMode::Dev, None, Some(OPT_OUT_PHRASE)).is_ok());
        assert!(check_boot(AuthMode::Dev, None, Some(&format!("  {OPT_OUT_PHRASE}  "))).is_ok());
        // Near misses are not the sentence. An opt-out that can be satisfied by `=1` is a flag.
        assert!(check_boot(AuthMode::Dev, None, Some("1")).is_err());
        assert!(check_boot(AuthMode::Dev, None, Some("true")).is_err());
        assert!(check_boot(AuthMode::Dev, None, Some("yes")).is_err());
    }
}
