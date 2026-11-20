//! Safety checks for fetching remote content, shared by every
//! feature that downloads bytes from a URL it did not itself
//! construct (link previews, GIF media, and similar).
//!
//! The check is deliberately conservative: the URL must be https,
//! and the host, whether a literal IP or every DNS answer for the
//! name, must be a public internet address. Anything else could
//! make the client ping internal network addresses and leak the
//! user's presence and IP to whoever supplied the URL (security
//! audit SEC-01; the same validation now also guards the GIPHY
//! media fetch, SEC-F02).

/// Whether an IP is a public internet address. Anything else
/// (private, loopback, link-local, unspecified, multicast, the
/// special-purpose ranges, and the v6 equivalents) must never be
/// fetched by an automatic worker. IPv6 addresses that embed an
/// IPv4 address (the mapped and compatible forms) are classified
/// by the embedded address, so the v4 rules cannot be bypassed by
/// spelling the same host as IPv6.
#[must_use]
pub fn is_public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => is_public_v4(v4),
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let segments = v6.segments();
            if segments[..6] == [0; 6] {
                // IPv4-compatible form ::a.b.c.d. This range also
                // covers :: and ::1, whose embedded forms (0.0.0.0
                // and 0.0.0.1) the v4 rules reject.
                let embedded = std::net::Ipv4Addr::new(
                    (segments[6] >> 8) as u8,
                    segments[6] as u8,
                    (segments[7] >> 8) as u8,
                    segments[7] as u8,
                );
                return is_public_v4(embedded);
            }
            !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_multicast()
                && !v6.is_unique_local()
                && !v6.is_unicast_link_local()
                // 2001:db8::/32 is the documentation range.
                && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
                // fec0::/10 is the deprecated site-local range.
                && segments[0] & 0xffc0 != 0xfec0
                // 100::/64 is the discard-only prefix.
                && !(segments[0] == 0x0100 && segments[1..4] == [0; 3])
        }
    }
}

/// Whether an IPv4 address is a genuinely global unicast address:
/// not private, loopback, link-local or multicast, and not in any
/// special-purpose range that is never assigned to public hosts.
fn is_public_v4(v4: std::net::Ipv4Addr) -> bool {
    let octets = v4.octets();
    !v4.is_private()
        && !v4.is_loopback()
        && !v4.is_link_local()
        && !v4.is_unspecified()
        && !v4.is_multicast()
        && !v4.is_broadcast()
        // 0.0.0.0/8 is "this network"; the kernel treats the whole
        // block as local, not just 0.0.0.0 itself.
        && octets[0] != 0
        // 100.64.0.0/10 is the CGNAT shared address space, reachable
        // inside carrier and cloud networks but never public.
        && !(octets[0] == 100 && octets[1] & 0xc0 == 0x40)
        // 192.0.0.0/24 holds IETF special-purpose assignments.
        && !(octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        // The documentation ranges 192.0.2.0/24, 198.51.100.0/24
        // and 203.0.113.0/24 are never assigned to hosts.
        && !((octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
            || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
            || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113))
        // 198.18.0.0/15 is the benchmarking range.
        && !(octets[0] == 198 && octets[1] & 0xfe == 18)
        // 240.0.0.0/4 is reserved (the broadcast address inside it
        // is already excluded above).
        && octets[0] < 240
}

/// Extracts the host and port from an https URL without a
/// URL-parsing dependency, following the same authority handling
/// as [`crate::update`]'s download-URL check: userinfo is dropped
/// and IPv6 literals are unbracketed. The host is lowercased (DNS
/// names are case-insensitive, and fetchers key their address pin
/// by it) and internationalized names are converted to their
/// punycode A-labels, the form DNS resolvers and the HTTP stack's
/// own URL parser both use; without the conversion an IDN host
/// failed resolution here while the fetcher would have used the
/// A-labels, so such links were refused outright. The port
/// defaults to 443. Returns `None` for anything that is not a
/// well-formed https URL with a host and a valid port, and for
/// any URL containing a backslash before the fragment (see the
/// check in the body).
fn https_host_port(url: &str) -> Option<(String, u16)> {
    // The fetchers parse URLs with the WHATWG parser, which for
    // https also treats '\' as an authority delimiter; this parser
    // does not. On a URL containing one, the two parsers name
    // different hosts (the validator would check the name after
    // the last '@' while the fetcher connects to the host before
    // the '\'), which defeats the address pin. Rejecting '\'
    // anywhere before the fragment removes the disagreement by
    // construction: every accepted URL tokenizes identically in
    // both parsers, so the host validated here is the host the
    // fetcher connects to. The fragment is exempt because it is
    // never sent to a server and cannot move the authority.
    // Message and media URLs never legitimately contain a
    // backslash.
    let before_fragment = url.split('#').next().unwrap_or("");
    if before_fragment.contains('\\') {
        return None;
    }
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    // Authority runs to the first path/query/fragment delimiter.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Drop any userinfo, then split host and port.
    let host_port = authority.rsplit('@').next().unwrap_or("");
    let (host, port) = if let Some(v6) = host_port.strip_prefix('[') {
        let (host, rest) = v6.split_once(']')?;
        let port = match rest {
            "" => None,
            _ => Some(rest.strip_prefix(':')?),
        };
        (host, port)
    } else {
        match host_port.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        None => 443,
        Some(text) => text.parse().ok()?,
    };
    let host = host.to_lowercase();
    let host = if host.is_ascii() || host.parse::<std::net::IpAddr>().is_ok() {
        host
    } else {
        idna::domain_to_ascii(&host).ok()?
    };
    Some((host, port))
}

/// A remote-fetch target after validation: the host a fetch may
/// contact and every public address it resolved to at check time.
pub struct ValidatedTarget {
    /// The URL host (lowercased), the name the addresses belong to.
    pub host: String,
    /// Every validated public socket address for the host.
    pub addrs: Vec<std::net::SocketAddr>,
}

/// Resolves and validates a remote-fetch target in one step:
/// https only, and the host (literal or every DNS answer) must be
/// a public address. On success the caller pins its connection to
/// the returned addresses instead of letting the HTTP stack
/// resolve the name again at connect time; a second resolution
/// could answer differently and reopen the DNS rebinding window
/// the check is meant to close (SEC-F05). Callers that follow
/// redirects must validate every hop and pin each hop to its own
/// target.
#[must_use]
pub fn validated_target(url: &str) -> Option<ValidatedTarget> {
    use std::net::ToSocketAddrs;
    let (host, port) = https_host_port(url)?;
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if !is_public_ip(ip) {
            return None;
        }
        return Some(ValidatedTarget { host, addrs: vec![std::net::SocketAddr::new(ip, port)] });
    }
    let addrs: Vec<std::net::SocketAddr> = (host.as_str(), port).to_socket_addrs().ok()?.collect();
    if addrs.is_empty() || addrs.iter().any(|addr| !is_public_ip(addr.ip())) {
        return None;
    }
    Some(ValidatedTarget { host, addrs })
}

/// Validates a remote-fetch target: https only, and the host
/// (literal or every DNS answer) must be a public address. This
/// is the check-only form of [`validated_target`]; fetchers that
/// open a connection should prefer that one and pin to its
/// addresses. Callers that follow redirects must re-validate
/// every hop.
#[must_use]
pub fn public_https_url_allowed(url: &str) -> bool {
    validated_target(url).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("ip literal")
    }

    #[test]
    fn public_ip_classification() {
        assert!(is_public_ip(ip("8.8.8.8")));
        assert!(is_public_ip(ip("2606:4700:4700::1111")));
        for bad in [
            "127.0.0.1",
            "10.0.0.5",
            "172.16.0.9",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "ff02::1",
        ] {
            assert!(!is_public_ip(ip(bad)), "{bad} must not be public");
        }
    }

    #[test]
    fn ipv4_special_purpose_ranges_are_not_public() {
        for bad in [
            // 0.0.0.0/8, "this network".
            "0.1.2.3",
            "0.0.0.1",
            // 100.64.0.0/10, CGNAT shared address space.
            "100.64.0.1",
            "100.127.255.254",
            // 192.0.0.0/24, IETF special-purpose assignments.
            "192.0.0.1",
            // Documentation ranges.
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            // 198.18.0.0/15, benchmarking.
            "198.18.0.1",
            "198.19.255.1",
            // 240.0.0.0/4, reserved.
            "240.0.0.1",
            "250.1.2.3",
        ] {
            assert!(!is_public_ip(ip(bad)), "{bad} must not be public");
        }
        for good in [
            // Just outside the CGNAT /10 on both sides.
            "100.63.255.255",
            "100.128.0.1",
            // Just outside the benchmarking /15.
            "198.20.0.1",
            // 192.0.1.0/24 is ordinary allocated space.
            "192.0.1.1",
            // Top of the ordinary unicast range.
            "223.255.255.1",
        ] {
            assert!(is_public_ip(ip(good)), "{good} must be public");
        }
    }

    #[test]
    fn ipv6_embedded_ipv4_is_classified_by_the_embedded_address() {
        for bad in [
            // IPv4-mapped forms of non-public addresses.
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.5",
            "::ffff:192.168.1.1",
            "::ffff:169.254.169.254",
            "::ffff:100.64.0.1",
            "::ffff:0.1.2.3",
            // IPv4-compatible forms of the same.
            "::127.0.0.1",
            "::10.0.0.5",
            "::192.168.1.1",
        ] {
            assert!(!is_public_ip(ip(bad)), "{bad} must not be public");
        }
        // A mapped or compatible spelling of a public address
        // stays public: it names that same public host.
        assert!(is_public_ip(ip("::ffff:8.8.8.8")));
        assert!(is_public_ip(ip("::8.8.8.8")));
    }

    #[test]
    fn ipv6_special_purpose_ranges_are_not_public() {
        for bad in [
            // 2001:db8::/32, documentation.
            "2001:db8::1",
            // fec0::/10, deprecated site-local.
            "fec0::1",
            // 100::/64, discard-only.
            "100::1",
        ] {
            assert!(!is_public_ip(ip(bad)), "{bad} must not be public");
        }
    }

    #[test]
    fn url_validation_accepts_public_https_literals() {
        assert!(public_https_url_allowed("https://8.8.8.8/x.gif"));
        assert!(public_https_url_allowed("https://8.8.8.8:8443/x.gif"));
        assert!(public_https_url_allowed("https://[2606:4700:4700::1111]/x.gif"));
        // Userinfo does not change which host is contacted.
        assert!(public_https_url_allowed("https://user@8.8.8.8/x.gif"));
    }

    #[test]
    fn validated_target_pins_literal_addresses() {
        // The pin carries the URL's own port, defaulting to 443.
        let target = validated_target("https://8.8.8.8/x.gif").expect("public literal");
        assert_eq!(target.host, "8.8.8.8");
        assert_eq!(target.addrs, vec![std::net::SocketAddr::new(ip("8.8.8.8"), 443)]);
        let target = validated_target("https://8.8.8.8:8443/x.gif").expect("public literal");
        assert_eq!(target.addrs.len(), 1);
        assert_eq!(target.addrs[0].port(), 8443);
        let target =
            validated_target("https://[2606:4700:4700::1111]:8443/x").expect("public v6 literal");
        assert_eq!(target.host, "2606:4700:4700::1111");
        assert_eq!(target.addrs[0].ip(), ip("2606:4700:4700::1111"));
        assert_eq!(target.addrs[0].port(), 8443);
        // Userinfo does not change which host is pinned.
        let target = validated_target("https://user@8.8.8.8/x").expect("public literal");
        assert_eq!(target.host, "8.8.8.8");
    }

    #[test]
    fn idn_hosts_are_converted_to_punycode() {
        let (host, port) = https_host_port("https://münchen.de/preview").expect("idn host");
        assert_eq!(host, "xn--mnchen-3ya.de");
        assert_eq!(port, 443);
        // ASCII hosts pass through untouched (and lowercased).
        let (host, _) = https_host_port("https://EXAMPLE.com/x").expect("ascii host");
        assert_eq!(host, "example.com");
    }

    #[test]
    fn validated_target_rejects_what_it_cannot_pin() {
        for bad in [
            "http://8.8.8.8/x.gif",
            "https://127.0.0.1/admin",
            "https://169.254.169.254/latest/meta-data",
            "https://example.com@127.0.0.1/x",
            // A port outside u16 range can never be connected to.
            "https://8.8.8.8:99999/x",
            "not a url",
            "https://",
            "",
        ] {
            assert!(validated_target(bad).is_none(), "{bad} must be rejected");
        }
    }

    #[test]
    fn url_validation_rejects_ipv6_spellings_of_private_addresses() {
        for bad in [
            "https://[::ffff:127.0.0.1]/x",
            "https://[::ffff:10.0.0.5]/x",
            "https://[::127.0.0.1]/x",
            "https://[2001:db8::1]/x",
        ] {
            assert!(!public_https_url_allowed(bad), "{bad} must be rejected");
            assert!(validated_target(bad).is_none(), "{bad} must be rejected");
        }
        // The mapped spelling of a public address is allowed and
        // pins the address that was validated.
        let target =
            validated_target("https://[::ffff:8.8.8.8]/x.gif").expect("public mapped literal");
        assert_eq!(target.addrs[0].ip(), ip("::ffff:8.8.8.8"));
    }

    #[test]
    fn url_validation_rejects_backslashes_before_the_fragment() {
        for bad in [
            // The WHATWG parser ends the authority at the '\', so
            // it would connect to the host before it while this
            // validator used to check the name after the last '@'.
            "https://127.0.0.1\\@example.com/x",
            "https://example.com\\@127.0.0.1/x",
            "https://8.8.8.8\\@example.com/x",
            // Here the fetcher would connect to example.com but
            // the pin would have been keyed to 8.8.8.8's addresses.
            "https://example.com\\@8.8.8.8/x",
            // A backslash anywhere before the fragment is refused,
            // even in the path, so the parsers can never diverge.
            "https://8.8.8.8/a\\b",
            "https://example.com\\x",
        ] {
            assert!(!public_https_url_allowed(bad), "{bad} must be rejected");
            assert!(validated_target(bad).is_none(), "{bad} must be rejected");
        }
        // A backslash inside the fragment is never sent to a
        // server and cannot move the authority; it stays allowed.
        assert!(public_https_url_allowed("https://8.8.8.8/x#a\\b"));
    }

    #[test]
    fn url_validation_rejects_non_https_and_non_public() {
        for bad in [
            "http://8.8.8.8/x.gif",
            "ftp://8.8.8.8/x.gif",
            "https://127.0.0.1/admin",
            "https://169.254.169.254/latest/meta-data",
            "https://10.0.0.5:8443/x",
            "https://192.168.0.1/x",
            "https://[::1]/x",
            "https://[fe80::1]/x",
            // The host is what follows the userinfo, not what
            // precedes it.
            "https://example.com@127.0.0.1/x",
            "https://8.8.8.8@10.0.0.5/x",
            "not a url",
            "https://",
            "",
        ] {
            assert!(!public_https_url_allowed(bad), "{bad} must be rejected");
        }
    }
}
