//! The only form a client's address takes past the HTTP handler: its /24
//! (IPv4) or /48 (IPv6) network (PLAN.md Phase 6, rule 9). The address
//! lookups and the keyed tag take a `NetworkPrefix` and nothing else, so
//! nothing can look up or keep the full address.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetworkPrefix(Inner);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Inner {
    V4([u8; 3]),
    V6([u8; 6]),
}

impl NetworkPrefix {
    /// The network `ip` belongs to: an IPv4-mapped IPv6 address is taken
    /// as the IPv4 address it carries.
    pub fn of(ip: IpAddr) -> Self {
        match ip.to_canonical() {
            IpAddr::V4(v4) => {
                let [a, b, c, _] = v4.octets();
                Self(Inner::V4([a, b, c]))
            }
            IpAddr::V6(v6) => {
                let mut prefix = [0u8; 6];
                prefix.copy_from_slice(&v6.octets()[..6]);
                Self(Inner::V6(prefix))
            }
        }
    }

    /// The network's first address, for the address files: `a.b.c.0`, or
    /// the /48 followed by zeros.
    pub(crate) fn network(&self) -> IpAddr {
        match self.0 {
            Inner::V4([a, b, c]) => IpAddr::V4(Ipv4Addr::new(a, b, c, 0)),
            Inner::V6(prefix) => {
                let mut octets = [0u8; 16];
                octets[..6].copy_from_slice(&prefix);
                IpAddr::V6(Ipv6Addr::from(octets))
            }
        }
    }

    /// What the keyed tag is computed over: the family, then the prefix.
    pub(crate) fn tag_input(&self) -> Vec<u8> {
        match self.0 {
            Inner::V4(prefix) => [&[4u8][..], &prefix].concat(),
            Inner::V6(prefix) => [&[6u8][..], &prefix].concat(),
        }
    }
}

/// Nothing prints a network, so it can't reach a log by accident.
impl fmt::Debug for NetworkPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NetworkPrefix(hidden)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix(ip: &str) -> NetworkPrefix {
        NetworkPrefix::of(ip.parse().unwrap())
    }

    #[test]
    fn addresses_become_their_24_or_48() {
        assert_eq!(prefix("203.0.113.77").network().to_string(), "203.0.113.0");
        assert_eq!(prefix("203.0.113.77"), prefix("203.0.113.1"));
        assert_ne!(prefix("203.0.113.77"), prefix("203.0.114.77"));
        assert_eq!(
            prefix("2001:db8:1234:5678::9").network().to_string(),
            "2001:db8:1234::"
        );
        assert_eq!(
            prefix("2001:db8:1234:5678::9"),
            prefix("2001:db8:1234:ffff::1")
        );
        assert_ne!(prefix("2001:db8:1234::1"), prefix("2001:db8:1235::1"));
        // An IPv4-mapped address is the IPv4 address it carries.
        assert_eq!(prefix("::ffff:203.0.113.77"), prefix("203.0.113.5"));
        assert_ne!(
            prefix("203.0.113.0").tag_input(),
            prefix("2001:db8::").tag_input()
        );
    }

    #[test]
    fn a_prefix_never_prints() {
        assert_eq!(
            format!("{:?}", prefix("203.0.113.77")),
            "NetworkPrefix(hidden)"
        );
    }
}
