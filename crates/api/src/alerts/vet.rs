//! Where an alert is allowed to be sent.
//!
//! A webhook URL is operator-supplied and the server fetches it, which is the classic SSRF shape:
//! `http://169.254.169.254/…` turns the alerting feature into a cloud-metadata reader, and
//! `http://10.0.0.5/admin` turns it into a probe of the VPC the API happens to sit in. So a
//! destination must be:
//!
//! * `https://` — plaintext would put a signed alert body (which names projects, models and spend)
//!   on the wire in the clear. Loopback over `http` is allowed only in dev mode.
//! * a **public** address. Every resolved address is checked, not just the first: a hostname that
//!   resolves to one public and one private address is the standard rebinding trick.
//!
//! The check runs at **configure** time (so a bad channel is refused with a 400 that says why) and
//! again at **delivery** time (so a hostname that starts resolving to a private address later does
//! not become a door). Redirects are separately refused by the client's `Policy::none()` — a 302 to
//! `http://169.254.169.254` would otherwise walk straight past everything above.

use std::net::IpAddr;

/// Env: allow `http://` and loopback destinations. For local development only — it disables the
/// scheme and private-address checks for loopback targets.
const ENV_DEV_DESTINATIONS: &str = "LIGHTTRACK_ALERT_ALLOW_LOOPBACK";

/// Why a destination was refused. The message is operator-facing: it says what to change.
pub(crate) type Refusal = String;

/// Is dev-mode destination relaxation on?
pub(crate) fn dev_destinations() -> bool {
    matches!(
        std::env::var(ENV_DEV_DESTINATIONS).as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// The scheme/shape half of the check — no DNS, so it is safe to call on a request path.
pub(crate) fn check_scheme(url: &str, dev: bool) -> Result<(), Refusal> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if url.starts_with("http://") {
        if dev && is_loopback_host(host_of(url).unwrap_or_default()) {
            return Ok(());
        }
        return Err(format!(
            "alert destination '{url}' must be https:// — a plaintext webhook puts the alert body \
             (projects, models, spend) on the wire in the clear. Set {ENV_DEV_DESTINATIONS}=1 to \
             allow http://localhost in development."
        ));
    }
    Err(format!("alert destination '{url}' must be an http(s) URL"))
}

/// The full check: scheme, then every address the host resolves to.
///
/// Async because it resolves; call it off the request path where you can, but it is cheap enough
/// (one `getaddrinfo`, cached by the OS) to run before each delivery, which is the point — a
/// hostname re-pointed at `10.0.0.5` after configuration must not become a door.
pub(crate) async fn check(url: &str, dev: bool) -> Result<(), Refusal> {
    check_scheme(url, dev)?;
    let Some(host) = host_of(url) else {
        return Err(format!("alert destination '{url}' has no host"));
    };
    if dev && is_loopback_host(host) {
        return Ok(());
    }
    let port = default_port(url);
    let addrs = tokio::net::lookup_host((host.to_string(), port))
        .await
        .map_err(|e| format!("alert destination '{url}' does not resolve: {e}"))?;
    let mut any = false;
    for sa in addrs {
        any = true;
        if let Some(why) = refuse_ip(sa.ip()) {
            return Err(format!(
                "alert destination '{url}' resolves to {} ({why}). Alerts are only delivered to \
                 public addresses — a private or link-local destination would turn alert delivery \
                 into a probe of the network this server happens to sit in.",
                sa.ip()
            ));
        }
    }
    if !any {
        return Err(format!("alert destination '{url}' resolves to no address"));
    }
    Ok(())
}

/// `Some(reason)` when this address must never be fetched.
fn refuse_ip(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                Some("loopback")
            } else if v4.is_private() {
                Some("private")
            } else if v4.is_link_local() {
                // 169.254.0.0/16 — where every cloud parks its instance-metadata service.
                Some("link-local / cloud metadata")
            } else if v4.is_broadcast() || v4.is_multicast() || v4.is_unspecified() {
                Some("not a unicast host")
            } else if v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]) {
                // 100.64.0.0/10 — carrier-grade NAT, and what several mesh VPNs hand out.
                Some("shared address space (CGNAT)")
            } else {
                None
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                Some("loopback")
            } else if v6.is_unspecified() || v6.is_multicast() {
                Some("not a unicast host")
            } else if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                Some("link-local")
            } else if (v6.segments()[0] & 0xfe00) == 0xfc00 {
                Some("unique-local")
            } else if let Some(v4) = v6.to_ipv4_mapped() {
                // An IPv4-mapped address is the same host by another spelling.
                refuse_ip(IpAddr::V4(v4))
            } else {
                None
            }
        }
    }
}

/// The host portion of an `http(s)://` URL, without port, userinfo, path or `[]` brackets.
fn host_of(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if let Some(end) = authority.strip_prefix('[').and_then(|r| r.find(']')) {
        return Some(&authority[1..=end]);
    }
    let host = authority.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

fn default_port(url: &str) -> u16 {
    let Some(host_part) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .and_then(|r| r.split(['/', '?', '#']).next())
    else {
        return 443;
    };
    // A bracketed IPv6 literal's colons are not a port separator.
    let after = host_part.rsplit_once(']').map_or(host_part, |(_, a)| a);
    match after.rsplit_once(':').and_then(|(_, p)| p.parse().ok()) {
        Some(p) => p,
        None if url.starts_with("http://") => 80,
        None => 443,
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_is_refused_and_the_message_names_the_escape_hatch() {
        let err = check_scheme("http://example.test/hook", false).expect_err("http refused");
        assert!(err.contains("must be https://"), "{err}");
        assert!(err.contains(ENV_DEV_DESTINATIONS), "{err}");
        assert!(check_scheme("https://example.test/hook", false).is_ok());
        assert!(check_scheme("ftp://example.test/hook", false).is_err());
        assert!(check_scheme("ops@example.test", false).is_err());
    }

    /// Dev mode relaxes loopback only — it is not a switch that turns the whole check off.
    #[test]
    fn dev_mode_allows_loopback_and_nothing_else() {
        assert!(check_scheme("http://localhost:8080/hook", true).is_ok());
        assert!(check_scheme("http://127.0.0.1:8080/hook", true).is_ok());
        assert!(
            check_scheme("http://10.0.0.5/hook", true).is_err(),
            "dev mode is for localhost, not for the VPC"
        );
    }

    #[test]
    fn every_address_family_that_must_never_be_fetched_is_refused() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            // The one that matters most: cloud instance metadata.
            "169.254.169.254",
            "100.100.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:10.0.0.1",
        ] {
            let parsed: IpAddr = ip.parse().expect(ip);
            assert!(refuse_ip(parsed).is_some(), "{ip} must be refused");
        }
        for ip in ["1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
            let parsed: IpAddr = ip.parse().expect(ip);
            assert!(refuse_ip(parsed).is_none(), "{ip} is public");
        }
    }

    #[test]
    fn the_host_is_extracted_without_userinfo_port_or_path() {
        assert_eq!(
            host_of("https://example.test/hook?x=1"),
            Some("example.test")
        );
        assert_eq!(
            host_of("https://example.test:8443/hook"),
            Some("example.test")
        );
        assert_eq!(
            host_of("https://user:pw@example.test/hook"),
            Some("example.test")
        );
        assert_eq!(
            host_of("https://[2606:4700::1111]:8443/h"),
            Some("2606:4700::1111")
        );
        assert_eq!(host_of("https:///nohost"), None);
        assert_eq!(host_of("ftp://example.test"), None);
    }

    #[test]
    fn the_port_defaults_by_scheme_and_survives_an_ipv6_literal() {
        assert_eq!(default_port("https://example.test/h"), 443);
        assert_eq!(default_port("http://example.test/h"), 80);
        assert_eq!(default_port("https://example.test:8443/h"), 8443);
        assert_eq!(default_port("https://[2606:4700::1111]/h"), 443);
        assert_eq!(default_port("https://[2606:4700::1111]:9000/h"), 9000);
    }

    /// The resolving check must refuse a private destination even when the URL looks innocent.
    #[tokio::test]
    async fn a_private_literal_is_refused_by_the_resolving_check() {
        let err = check("https://10.0.0.5/hook", false)
            .await
            .expect_err("private refused");
        assert!(err.contains("private"), "{err}");
        let err = check("https://169.254.169.254/latest/meta-data", false)
            .await
            .expect_err("metadata refused");
        assert!(err.contains("metadata"), "{err}");
    }
}
