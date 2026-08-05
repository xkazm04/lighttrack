//! Who is calling — the part of a throttle an attacker gets to lie about.
//!
//! The default identity is the **socket peer address**, because it is the only one a client cannot
//! choose for itself. `X-Forwarded-For` is not trusted unless `LIGHTTRACK_AUTH_TRUSTED_PROXY_HOPS`
//! states how many proxies sit in front of this instance (default `0` = never). A blindly-trusted
//! XFF hands an attacker both halves of the failure: **evasion** (a fresh fake address per guess, so
//! the budget never runs out) and **poisoning** (fake a victim's address into a lockout).
//!
//! IPv6 is bucketed to its /64: a single subscriber routinely holds a whole one, so counting per
//! address would be a free bypass for anyone with native IPv6.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};

use axum::extract::{ConnectInfo, Request};
use axum::http::HeaderMap;

/// The throttle key for a request, or `None` when the caller cannot be identified at all.
pub(super) fn of(req: &Request, trusted_hops: usize) -> Option<String> {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>()?.0.ip();
    if trusted_hops == 0 {
        return Some(bucket(peer));
    }
    Some(bucket(
        forwarded_for(req.headers(), trusted_hops).unwrap_or(peer),
    ))
}

/// The client address according to a chain of `hops` **trusted** proxies.
///
/// Each proxy appends the address it received from, so the rightmost `hops` entries are the ones our
/// trusted chain wrote, and the leftmost of those is the address the outermost trusted proxy actually
/// saw. Everything to its left was written by something we do not trust and is ignored. A list
/// shorter than the promised chain did not come through it at all — `None`, and the caller falls back
/// to the socket peer.
fn forwarded_for(headers: &HeaderMap, hops: usize) -> Option<IpAddr> {
    let list: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let entry = list.get(list.len().checked_sub(hops)?)?;
    // A bare address, or the `host:port` form some proxies append (including bracketed IPv6).
    // Anything that is not an address at all falls back to the peer rather than becoming a map key.
    entry
        .parse::<IpAddr>()
        .ok()
        .or_else(|| entry.parse::<SocketAddr>().ok().map(|sa| sa.ip()))
}

fn bucket(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            // A dual-stack listener reports IPv4 peers in mapped form; count them as the IPv4 they are.
            Some(v4) => v4.to_string(),
            None => {
                let mut prefix = [0u8; 16];
                prefix[..8].copy_from_slice(&v6.octets()[..8]);
                format!("{}/64", Ipv6Addr::from(prefix))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::HeaderValue;

    fn req_with(peer: &str, xff: Option<&str>) -> Request {
        let mut req = Request::new(Body::empty());
        req.extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        if let Some(v) = xff {
            req.headers_mut()
                .insert("x-forwarded-for", HeaderValue::from_str(v).unwrap());
        }
        req
    }

    #[test]
    fn x_forwarded_for_is_ignored_unless_hops_are_configured() {
        // The default posture: the header is noise, the peer is the identity. An attacker can
        // neither rotate out of their bucket nor forge a victim into one.
        assert_eq!(
            of(&req_with("203.0.113.9:5000", Some("198.51.100.7")), 0).as_deref(),
            Some("203.0.113.9")
        );
    }

    #[test]
    fn one_trusted_hop_takes_the_address_that_proxy_appended() {
        // The proxy appends last, so the rightmost entry is the only trustworthy one — the two
        // entries the client invented to its left must not be reachable.
        assert_eq!(
            of(
                &req_with("10.0.0.1:5000", Some("1.1.1.1, 2.2.2.2, 198.51.100.7")),
                1
            )
            .as_deref(),
            Some("198.51.100.7")
        );
        // Two hops in, two written by the chain: the client's real address is the leftmost of those.
        assert_eq!(
            of(
                &req_with("10.0.0.1:5000", Some("1.1.1.1, 198.51.100.7, 10.0.0.9")),
                2
            )
            .as_deref(),
            Some("198.51.100.7")
        );
        // Two hops promised, one entry delivered: the header did not come through the chain, so it
        // is discarded rather than believed.
        assert_eq!(
            of(&req_with("10.0.0.1:5000", Some("198.51.100.7")), 2).as_deref(),
            Some("10.0.0.1")
        );
        // Garbage in a trusted position falls back to the peer instead of becoming a map key.
        assert_eq!(
            of(&req_with("10.0.0.1:5000", Some("not-an-address")), 1).as_deref(),
            Some("10.0.0.1")
        );
        // A port-suffixed entry is still an address.
        assert_eq!(
            of(&req_with("10.0.0.1:5000", Some("198.51.100.7:4321")), 1).as_deref(),
            Some("198.51.100.7")
        );
    }

    #[test]
    fn ipv6_counts_per_64_and_mapped_v4_counts_as_v4() {
        // A subscriber holds a whole /64; per-address counting would be a free bypass.
        assert_eq!(
            bucket("2001:db8:1:2:aaaa:bbbb:cccc:dddd".parse().unwrap()),
            bucket("2001:db8:1:2:0:0:0:1".parse().unwrap())
        );
        assert_ne!(
            bucket("2001:db8:1:2::1".parse().unwrap()),
            bucket("2001:db8:1:3::1".parse().unwrap())
        );
        assert_eq!(bucket("::ffff:203.0.113.9".parse().unwrap()), "203.0.113.9");
    }

    #[test]
    fn no_connect_info_means_no_source_and_therefore_no_throttling() {
        // Fail open rather than collapse every caller into one shared bucket, which would be a
        // lockout vector instead of a control.
        assert_eq!(of(&Request::new(Body::empty()), 0), None);
    }
}
