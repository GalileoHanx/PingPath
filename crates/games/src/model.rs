use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

use ipnet::IpNet;
use serde::Deserialize;

/// A game definition, as stored in `games/<id>.yaml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameDefinition {
    pub id: String,
    pub name: String,
    pub status: SupportStatus,
    #[serde(default)]
    pub status_reason: Option<String>,
    /// Compatibility report, relative to the repository root.
    #[serde(default)]
    pub report: Option<String>,
    pub anti_cheat: Vec<AntiCheat>,
    pub ports: Ports,
    /// Steam app ID, for games on Valve's Steam Datagram Relay.
    #[serde(default)]
    pub sdr_app_id: Option<u32>,
    pub regions: Vec<Region>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Anti-cheat compatibility status (PLAN.md section 2, rule 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SupportStatus {
    Untested,
    Testing,
    Supported,
    Unsupported,
}

/// What the client may do with a game's traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingPolicy {
    /// Never route; traffic stays on the direct path.
    Refuse,
    /// Route, but show the player a warning first.
    Warn,
    Allow,
}

impl SupportStatus {
    pub fn routing_policy(self) -> RoutingPolicy {
        match self {
            Self::Untested | Self::Unsupported => RoutingPolicy::Refuse,
            Self::Testing => RoutingPolicy::Warn,
            Self::Supported => RoutingPolicy::Allow,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Untested => "untested",
            Self::Testing => "testing",
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
        }
    }
}

impl fmt::Display for SupportStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AntiCheat {
    None,
    Vac,
    EasyAntiCheat,
    Battleye,
    Vanguard,
    FaceitAc,
    Ricochet,
    Other,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ports {
    #[serde(default)]
    pub udp: Vec<PortRange>,
    #[serde(default)]
    pub tcp: Vec<PortRange>,
}

/// An inclusive port range; a single port has `first == last`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "PortSpec")]
pub struct PortRange {
    pub first: u16,
    pub last: u16,
}

impl PortRange {
    pub fn contains(&self, port: u16) -> bool {
        (self.first..=self.last).contains(&port)
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.first == self.last {
            write!(f, "{}", self.first)
        } else {
            write!(f, "{}-{}", self.first, self.last)
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PortSpec {
    Single(u64),
    Range(String),
}

impl TryFrom<PortSpec> for PortRange {
    type Error = String;

    fn try_from(spec: PortSpec) -> Result<Self, Self::Error> {
        let port = |n: u64| match u16::try_from(n) {
            Ok(p) if p != 0 => Ok(p),
            _ => Err(format!("port {n} is outside 1-65535")),
        };
        match spec {
            PortSpec::Single(n) => {
                let p = port(n)?;
                Ok(Self { first: p, last: p })
            }
            PortSpec::Range(s) => {
                let (first, last) = s
                    .split_once('-')
                    .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)))
                    .ok_or_else(|| {
                        format!("\"{s}\" is not a port or a range like \"27015-27050\"")
                    })?;
                let (first, last) = (port(first)?, port(last)?);
                if first > last {
                    return Err(format!("port range \"{s}\" is reversed"));
                }
                Ok(Self { first, last })
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Region {
    pub id: String,
    pub name: String,
    pub server_ranges: Vec<IpNet>,
    #[serde(default)]
    pub ping_targets: Vec<PingTarget>,
    /// Valve relay points of presence serving this region (e.g. `fra`).
    #[serde(default)]
    pub sdr_pops: Vec<String>,
    #[serde(default)]
    pub location: Option<Location>,
}

/// Where a region's servers are.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Location {
    pub lat: f64,
    pub lon: f64,
}

/// Something that answers latency probes: an address (ICMP echo) or an
/// address and port (TCP connection time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum PingTarget {
    Icmp(IpAddr),
    Tcp(SocketAddr),
}

impl PingTarget {
    pub fn ip(&self) -> IpAddr {
        match self {
            Self::Icmp(ip) => *ip,
            Self::Tcp(addr) => addr.ip(),
        }
    }
}

impl FromStr for PingTarget {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(ip) = s.parse() {
            return Ok(Self::Icmp(ip));
        }
        match s.parse::<SocketAddr>() {
            Ok(addr) if addr.port() == 0 => Err(format!("\"{s}\": port 0 is not a valid target")),
            Ok(addr) => Ok(Self::Tcp(addr)),
            Err(_) => Err(format!(
                "\"{s}\" is not an address or address:port (IPv6: [address]:port)"
            )),
        }
    }
}

impl TryFrom<String> for PingTarget {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl fmt::Display for PingTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Icmp(ip) => write!(f, "{ip}"),
            Self::Tcp(addr) => write!(f, "{addr}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(spec: PortSpec) -> Result<PortRange, String> {
        PortRange::try_from(spec)
    }

    #[test]
    fn parses_single_ports_and_ranges() {
        assert_eq!(
            range(PortSpec::Single(443)),
            Ok(PortRange {
                first: 443,
                last: 443
            })
        );
        assert_eq!(
            range(PortSpec::Range("27015-27050".into())),
            Ok(PortRange {
                first: 27015,
                last: 27050
            })
        );
    }

    #[test]
    fn rejects_bad_ports() {
        assert!(range(PortSpec::Single(0)).is_err());
        assert!(range(PortSpec::Single(65536)).is_err());
        assert!(range(PortSpec::Range("70000-70001".into())).is_err());
        assert!(
            range(PortSpec::Range("27050-27015".into()))
                .unwrap_err()
                .contains("reversed")
        );
        assert!(range(PortSpec::Range("27015".into())).is_err());
    }

    #[test]
    fn parses_ping_targets() {
        assert_eq!(
            "1.1.1.1".parse(),
            Ok(PingTarget::Icmp("1.1.1.1".parse().unwrap()))
        );
        assert_eq!(
            "1.1.1.1:443".parse(),
            Ok(PingTarget::Tcp("1.1.1.1:443".parse().unwrap()))
        );
        assert_eq!(
            "[2606:4700::1111]:443".parse::<PingTarget>().unwrap().ip(),
            "2606:4700::1111".parse::<IpAddr>().unwrap()
        );
        assert!(
            "1.1.1.1:0"
                .parse::<PingTarget>()
                .unwrap_err()
                .contains("port 0")
        );
        assert!("example.com:443".parse::<PingTarget>().is_err());
    }

    #[test]
    fn only_tested_games_route_without_warning() {
        assert_eq!(
            SupportStatus::Untested.routing_policy(),
            RoutingPolicy::Refuse
        );
        assert_eq!(
            SupportStatus::Unsupported.routing_policy(),
            RoutingPolicy::Refuse
        );
        assert_eq!(SupportStatus::Testing.routing_policy(), RoutingPolicy::Warn);
        assert_eq!(
            SupportStatus::Supported.routing_policy(),
            RoutingPolicy::Allow
        );
    }
}
