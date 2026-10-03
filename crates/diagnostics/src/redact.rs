//! Shortening of identifying data before it leaves the machine or a relay.
//!
//! Public IP addresses are cut to their /24 (IPv4) or /48 (IPv6) network,
//! the same truncation the crowd map uses (PLAN.md Phase 6), and written as
//! `203.0.113.x` or `2001:db8:1:x`. That form no longer parses as an address,
//! so redacting text twice changes nothing. Private,
//! loopback, link-local, carrier-grade NAT and documentation addresses
//! identify no one and are useful for diagnosing home networks, so they are
//! kept as they are.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Returns `ip` as text, shortened to its network if it is a public address.
pub fn redact_ip(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => redact_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => redact_v4(v4),
            None => redact_v6(v6),
        },
    }
}

fn redact_v4(ip: Ipv4Addr) -> String {
    let [a, b, c, _] = ip.octets();
    let cgnat = a == 100 && (b & 0xc0) == 64;
    let keep = ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_documentation()
        || ip.is_broadcast()
        || ip.is_multicast()
        || cgnat;
    if keep {
        ip.to_string()
    } else {
        format!("{a}.{b}.{c}.x")
    }
}

fn redact_v6(ip: Ipv6Addr) -> String {
    let s = ip.segments();
    let global_unicast = (s[0] & 0xe000) == 0x2000;
    let documentation = s[0] == 0x2001 && s[1] == 0x0db8;
    if !global_unicast || documentation {
        ip.to_string()
    } else {
        format!("{:x}:{:x}:{:x}:x", s[0], s[1], s[2])
    }
}

/// Display wrapper for logging addresses, e.g.
/// `tracing::info!(peer = %Redacted(addr), "session started")`.
#[derive(Debug, Clone, Copy)]
pub struct Redacted<T>(pub T);

impl fmt::Display for Redacted<IpAddr> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&redact_ip(self.0))
    }
}

impl fmt::Display for Redacted<SocketAddr> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            SocketAddr::V4(a) => write!(f, "{}:{}", redact_ip(IpAddr::V4(*a.ip())), a.port()),
            SocketAddr::V6(a) => write!(f, "[{}]:{}", redact_ip(IpAddr::V6(*a.ip())), a.port()),
        }
    }
}

/// Shortens every public IP address (with or without a port) found in
/// `text`. Anything that doesn't parse as an address is left alone.
pub fn redact_text(text: &str) -> String {
    let is_addr_char = |c: char| c.is_ascii_hexdigit() || matches!(c, '.' | ':' | '[' | ']');
    let mut out = String::with_capacity(text.len());
    let mut token_start = None;

    for (i, c) in text.char_indices() {
        match (is_addr_char(c), token_start) {
            (true, None) => token_start = Some(i),
            (false, Some(start)) => {
                out.push_str(&redact_token(&text[start..i]));
                out.push(c);
                token_start = None;
            }
            (false, None) => out.push(c),
            (true, Some(_)) => {}
        }
    }
    if let Some(start) = token_start {
        out.push_str(&redact_token(&text[start..]));
    }
    out
}

fn redact_token(token: &str) -> String {
    let parsed = |candidate: &str| {
        if let Ok(ip) = candidate.parse::<IpAddr>() {
            Some(redact_ip(ip))
        } else {
            candidate
                .parse::<SocketAddr>()
                .ok()
                .map(|addr| Redacted(addr).to_string())
        }
    };
    if let Some(redacted) = parsed(token) {
        return redacted;
    }
    // Trailing punctuation ("... from 8.8.8.8.") is not part of the address.
    let trimmed = token.trim_end_matches(['.', ':']);
    match parsed(trimmed) {
        Some(redacted) => format!("{redacted}{}", &token[trimmed.len()..]),
        None => token.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses_are_shortened() {
        assert_eq!(redact_ip("8.8.4.4".parse().unwrap()), "8.8.4.x");
        assert_eq!(
            redact_ip("2a00:1450:4001:82a::200e".parse().unwrap()),
            "2a00:1450:4001:x"
        );
        assert_eq!(redact_ip("::ffff:8.8.4.4".parse().unwrap()), "8.8.4.x");
    }

    #[test]
    fn non_identifying_addresses_are_kept() {
        for ip in [
            "192.168.1.20",
            "10.0.0.1",
            "100.72.1.1",
            "127.0.0.1",
            "169.254.3.4",
            "203.0.113.9",
        ] {
            assert_eq!(redact_ip(ip.parse().unwrap()), ip);
        }
        for ip in ["::1", "fe80::1", "fd00::5", "2001:db8::1"] {
            assert_eq!(redact_ip(ip.parse().unwrap()), ip);
        }
    }

    #[test]
    fn socket_addresses_keep_their_port() {
        let v4: SocketAddr = "8.8.4.4:27015".parse().unwrap();
        assert_eq!(Redacted(v4).to_string(), "8.8.4.x:27015");
        let v6: SocketAddr = "[2a00:1450:4001:82a::200e]:443".parse().unwrap();
        assert_eq!(Redacted(v6).to_string(), "[2a00:1450:4001:x]:443");
    }

    #[test]
    fn addresses_inside_text_are_shortened() {
        assert_eq!(
            redact_text("route to 8.8.4.4 via 192.168.1.1, relay [2a00:1450:4001:82a::1]:51820."),
            "route to 8.8.4.x via 192.168.1.1, relay [2a00:1450:4001:x]:51820."
        );
        assert_eq!(
            redact_text("peer=1.1.1.1:443 done"),
            "peer=1.1.1.x:443 done"
        );
        assert_eq!(redact_text("last hop 8.8.8.8."), "last hop 8.8.8.x.");
        assert_eq!(
            redact_text("gw 2a00:1450:4001:82a:1::"),
            "gw 2a00:1450:4001:x"
        );
        assert_eq!(
            redact_text("route 155.133.226.0/24"),
            "route 155.133.226.x/24"
        );
    }

    #[test]
    fn redacting_twice_changes_nothing() {
        let once = redact_text("8.8.4.4:51820 [2a00:1450:4001:82a::1]:443 1.1.1.1/32");
        assert_eq!(redact_text(&once), once);
    }

    #[test]
    fn text_without_addresses_is_unchanged() {
        for text in [
            "2026-10-03T16:13:33.123456Z INFO started",
            "version 0.1.0 on C:\\Users\\player",
            "deadbeef cafe 16:13:33 ::",
            "",
        ] {
            assert_eq!(redact_text(text), text);
        }
    }
}
