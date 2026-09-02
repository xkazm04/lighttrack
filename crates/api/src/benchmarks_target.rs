//! Validating a benchmark's target matrix at the door: the comparison rows, the registry prompts
//! they name, and — for an `Http` target — where it is allowed to point.
//!
//! Split out of [`crate::benchmarks`] because a target stopped being inert data. It now describes
//! **outbound work the server will later cause**: a URL a worker POSTs each case to, and a registry
//! name a worker fetches content from. Both are refusable facts, and both are better refused when
//! the benchmark is written than discovered by the run that was supposed to gate a deploy.

use lighttrack_core::{url_host, BenchTarget, TargetKind};

use crate::error::ApiError;
use crate::state::{spawn_db, AppState};

/// Vet an `Http` target's URL before it is stored.
///
/// A benchmark target is operator-supplied and a worker will POST to it unattended, from inside the
/// deployment's network, once per case — which is a server-side request forgery primitive if it may
/// name anything. So: **https only** (a benchmark case carries the operator's prompts and reference
/// answers; plaintext is not a trade we make for them) and **no private, loopback, link-local or
/// otherwise internal address**, whether written as a literal or as a name we can recognise.
///
/// A name that resolves to a private address at *call* time still gets through — DNS is not ours to
/// pin here, and a check that pretended otherwise would be security theatre. This refuses the
/// straightforward mistakes and the straightforward abuses; the endpoint's own signature check
/// ([`lighttrack_engine::http_target`]) is what makes the traffic attributable.
pub(crate) fn vet_target_url(url: &str) -> Result<(), String> {
    let scheme = url.split_once("://").map(|(s, _)| s.to_ascii_lowercase());
    match scheme.as_deref() {
        Some("https") => {}
        Some(other) => {
            return Err(format!(
                "http target url must use https (got '{other}'): a benchmark case carries your \
                 prompts and reference answers"
            ))
        }
        None => return Err(format!("http target url '{url}' is not an absolute url")),
    }
    let host = url_host(url).ok_or_else(|| format!("http target url '{url}' has no host"))?;
    if let Some(reason) = private_host_reason(&host) {
        return Err(format!(
            "http target url host '{host}' is refused ({reason}); a benchmark target must be an \
             endpoint reachable as a public service, not an address inside the deployment"
        ));
    }
    Ok(())
}

/// Why `host` is not an acceptable benchmark target, or `None` when it is fine.
fn private_host_reason(host: &str) -> Option<&'static str> {
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        return ipv4_reason(ip);
    }
    if let Ok(ip) = host.parse::<std::net::Ipv6Addr>() {
        if let Some(v4) = ip.to_ipv4_mapped() {
            return ipv4_reason(v4).or(Some("ipv4-mapped private address"));
        }
        if ip.is_loopback() {
            return Some("loopback");
        }
        if ip.is_unspecified() {
            return Some("unspecified address");
        }
        if ip.is_multicast() {
            return Some("multicast");
        }
        let seg = ip.segments()[0];
        if seg & 0xfe00 == 0xfc00 {
            return Some("unique-local address");
        }
        if seg & 0xffc0 == 0xfe80 {
            return Some("link-local address");
        }
        return None;
    }
    // A name. Refuse the ones that mean "inside", including a bare single-label host — which on a
    // container network is exactly how a neighbouring service is addressed.
    if host == "localhost" {
        return Some("loopback name");
    }
    for suffix in [".localhost", ".local", ".internal", ".localdomain"] {
        if host.ends_with(suffix) {
            return Some("internal-only name suffix");
        }
    }
    if !host.contains('.') {
        return Some("single-label host name resolves only inside the local network");
    }
    None
}

fn ipv4_reason(ip: std::net::Ipv4Addr) -> Option<&'static str> {
    let [a, b, ..] = ip.octets();
    if ip.is_loopback() {
        return Some("loopback");
    }
    if ip.is_private() {
        return Some("private range");
    }
    if ip.is_link_local() {
        return Some("link-local (cloud metadata lives here)");
    }
    if ip.is_unspecified() {
        return Some("unspecified address");
    }
    if ip.is_multicast() || ip.is_broadcast() {
        return Some("multicast/broadcast");
    }
    if a == 100 && (64..128).contains(&b) {
        return Some("carrier-grade NAT range");
    }
    None
}

/// Validate the stored `target` field before it reaches the store.
///
/// An **array** is unambiguously a comparison matrix and must deserialize as `Vec<BenchTarget>`; a
/// malformed one is rejected here (400) rather than silently degrading to a different benchmark
/// mode at run time. Non-array targets (null / object / string) are legacy free-form and pass
/// through untouched. Returns the parsed matrix so the caller can check what it *names*.
pub(crate) fn validate_target_matrix(
    target: &serde_json::Value,
) -> Result<Vec<BenchTarget>, String> {
    if !target.is_array() {
        return Ok(Vec::new());
    }
    let targets: Vec<BenchTarget> = serde_json::from_value(target.clone()).map_err(|e| {
        format!(
            "`target` is an array but not a valid comparison matrix (expected [{{provider, model, \
             system_prompt?, label?, prompt_ref?, kind?}}, ...]): {e}"
        )
    })?;
    for t in &targets {
        if let Some(r) = &t.prompt_ref {
            r.validate()?;
        }
        if let TargetKind::Http { url } = &t.kind {
            vet_target_url(url)?;
        }
    }
    Ok(targets)
}

/// Every registry prompt this matrix names must exist in the benchmark's project.
///
/// A typo'd `prompt_ref` is otherwise found by the run that was supposed to gate a promotion — the
/// worst possible moment and the least legible error. Refused as a 400 at write time instead.
///
/// A backend without the prompt registry (`Unsupported` → 501) cannot answer, and refusing every
/// resolvable benchmark there would be worse than not checking: the run still resolves through the
/// API, which will 501 legibly. So an unsupported registry means "cannot verify", not "invalid".
pub(crate) async fn ensure_prompt_refs_exist(
    st: &AppState,
    project_id: &str,
    targets: &[BenchTarget],
) -> Result<(), ApiError> {
    let mut names: Vec<String> = targets
        .iter()
        .filter_map(|t| t.prompt_ref.as_ref().map(|r| r.name.clone()))
        .collect();
    names.sort();
    names.dedup();
    for name in names {
        let store = st.store.clone();
        let (pid, n) = (project_id.to_string(), name.clone());
        match spawn_db(move || store.get_prompt(&pid, &n)).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(ApiError::bad_request(format!(
                    "target prompt_ref names '{name}', which is not a prompt in this project"
                )))
            }
            Err(e) if e.is_unsupported() => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn non_array_targets_pass_through() {
        assert!(validate_target_matrix(&json!(null)).unwrap().is_empty());
        assert!(validate_target_matrix(&json!({ "endpoint": "https://x" }))
            .unwrap()
            .is_empty());
        assert!(validate_target_matrix(&json!("legacy")).unwrap().is_empty());
    }

    #[test]
    fn valid_matrix_ok_malformed_rejected() {
        assert_eq!(
            validate_target_matrix(&json!([{ "provider": "openai", "model": "gpt-4o" }]))
                .unwrap()
                .len(),
            1
        );
        // Missing required `provider` → rejected (would otherwise silently degrade to simple mode).
        assert!(validate_target_matrix(&json!([{ "model": "x" }])).is_err());
        assert!(validate_target_matrix(&json!(["nope"])).is_err());
    }

    #[test]
    fn a_contradictory_prompt_ref_is_refused_at_the_door() {
        let e = validate_target_matrix(&json!([{
            "provider": "openai", "model": "gpt-4o",
            "prompt_ref": { "name": "p", "version": 3, "label": "production" }
        }]))
        .expect_err("both version and label is ambiguous");
        assert!(e.contains("at most one"), "{e}");
        // One or the other is fine.
        assert!(validate_target_matrix(&json!([{
            "provider": "openai", "model": "gpt-4o",
            "prompt_ref": { "name": "support-reply", "label": "production" }
        }]))
        .is_ok());
    }

    #[test]
    fn an_http_target_must_be_https_and_public() {
        let http = |url: &str| {
            validate_target_matrix(&json!([{
                "provider": "acme", "model": "rag",
                "kind": { "type": "http", "url": url }
            }]))
        };
        assert!(http("https://rag.acme.com/answer").is_ok());
        assert!(http("http://rag.acme.com/answer").is_err(), "plaintext");
        assert!(http("/answer").is_err(), "relative");
    }

    #[test]
    fn the_addresses_that_make_this_an_ssrf_primitive_are_all_refused() {
        // A worker POSTs to this URL unattended from inside the deployment. Each of these is a way
        // to aim that at something that is not a benchmark target.
        for url in [
            "https://127.0.0.1/x",             // loopback
            "https://localhost/x",             // …by name
            "https://api.localhost/x",         // …by suffix
            "https://10.1.2.3/x",              // RFC1918
            "https://192.168.0.9/x",           //
            "https://172.16.4.4/x",            //
            "https://169.254.169.254/latest/", // cloud metadata — the classic
            "https://100.64.0.1/x",            // CGNAT
            "https://[::1]/x",                 // v6 loopback
            "https://[fd00::1]/x",             // v6 unique-local
            "https://[fe80::1]/x",             // v6 link-local
            "https://[::ffff:10.0.0.1]/x",     // v4-mapped private
            "https://db/x",                    // single-label container host
            "https://payments.internal/x",     // internal suffix
            "https://printer.local/x",         //
        ] {
            let e = vet_target_url(url).expect_err(url);
            assert!(
                e.contains("refused") || e.contains("https") || e.contains("absolute"),
                "{url}: {e}"
            );
        }
        // …while ordinary public endpoints are untouched.
        for url in [
            "https://rag.acme.com/answer",
            "https://8.8.8.8/answer",
            "https://eval.example.co.uk:8443/v1/answer",
        ] {
            assert!(vet_target_url(url).is_ok(), "must allow {url}");
        }
    }
}
