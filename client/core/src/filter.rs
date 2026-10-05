//! Which packets that reach the adapter go into the tunnel
//! (docs/decisions/008-split-tunnel-routing.md): IPv4 UDP to the region's
//! ranges and the game's ports, the same check the exit makes. The rest is
//! dropped and counted, so a too-broad game definition shows up in the UI
//! and the diagnostic report instead of failing silently.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU64, Ordering};

use pingpath_games::GameDefinition;
use pingpath_tunnel::packet::{self, PacketError};
use serde::Serialize;

/// Why a packet wasn't carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stray {
    /// To an address outside the region's ranges: the system's own chatter
    /// on a new interface (multicast, router solicitations). Logged, never
    /// shown as a game problem.
    NotGameDestination,
    /// IPv6 to a game range (exits don't forward IPv6 yet; routable games
    /// list IPv4 ranges only, so this means a definition changed).
    Ipv6NotCarried,
    /// TCP, ICMP or another protocol to a game range: exits carry only the
    /// game's UDP, so this traffic is blocked while optimized.
    NotUdp,
    /// UDP to a game range on a port the definition doesn't list.
    NotGamePort,
    Fragment,
    Malformed,
    /// From the tunnel, but not game UDP from the region's servers to the
    /// adapter: never written to the adapter.
    InboundNotGame,
}

impl Stray {
    pub const ALL: [Stray; 7] = [
        Stray::NotGameDestination,
        Stray::Ipv6NotCarried,
        Stray::NotUdp,
        Stray::NotGamePort,
        Stray::Fragment,
        Stray::Malformed,
        Stray::InboundNotGame,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Stray::NotGameDestination => "not_game_destination",
            Stray::Ipv6NotCarried => "ipv6_not_carried",
            Stray::NotUdp => "not_udp",
            Stray::NotGamePort => "not_game_port",
            Stray::Fragment => "fragment",
            Stray::Malformed => "malformed",
            Stray::InboundNotGame => "inbound_not_game",
        }
    }

    /// Whether the player should hear about it: game traffic that isn't
    /// carried, as opposed to the system's chatter or junk.
    pub fn affects_game(self) -> bool {
        matches!(
            self,
            Stray::Ipv6NotCarried | Stray::NotUdp | Stray::NotGamePort
        )
    }
}

/// Decides, for one game and region, which packets go into the tunnel and
/// which come out of it.
#[derive(Debug, Clone)]
pub struct PacketFilter {
    game: GameDefinition,
    region: String,
    adapter: Ipv4Addr,
}

impl PacketFilter {
    pub fn new(game: GameDefinition, region: &str, adapter: Ipv4Addr) -> Self {
        Self {
            game,
            region: region.to_string(),
            adapter,
        }
    }

    fn in_region(&self, ip: IpAddr) -> bool {
        self.game
            .region(&self.region)
            .is_some_and(|r| r.server_ranges.iter().any(|net| net.contains(&ip)))
    }

    /// `Ok(())` to carry a packet from the adapter into the tunnel, or why
    /// not. Destination first, so only traffic to the game's servers can
    /// count as a game problem.
    pub fn check(&self, packet: &[u8]) -> Result<(), Stray> {
        let dst = match packet.first().map(|b| b >> 4) {
            Some(4) if packet.len() >= 20 => IpAddr::V4(Ipv4Addr::new(
                packet[16], packet[17], packet[18], packet[19],
            )),
            Some(6) if packet.len() >= 40 => {
                let octets: [u8; 16] = packet[24..40].try_into().expect("16 bytes");
                let dst = IpAddr::V6(Ipv6Addr::from(octets));
                return Err(if self.in_region(dst) {
                    Stray::Ipv6NotCarried
                } else {
                    Stray::NotGameDestination
                });
            }
            _ => return Err(Stray::Malformed),
        };
        if !self.in_region(dst) {
            return Err(Stray::NotGameDestination);
        }
        let udp = packet::parse_udp(packet).map_err(|e| match e {
            PacketError::NotIpv4(_) | PacketError::Malformed => Stray::Malformed,
            PacketError::Fragment => Stray::Fragment,
            PacketError::NotUdp(_) => Stray::NotUdp,
        })?;
        if self
            .game
            .allows_udp(&self.region, std::net::SocketAddr::V4(udp.dst))
        {
            Ok(())
        } else {
            Err(Stray::NotGamePort)
        }
    }

    /// `Ok(())` to write a packet from the tunnel to the adapter: IPv4 UDP
    /// from the region's servers and the game's ports, to the adapter's own
    /// address. Anything else from the network is never let in.
    pub fn check_inbound(&self, packet: &[u8]) -> Result<(), Stray> {
        let udp = packet::parse_udp(packet).map_err(|_| Stray::InboundNotGame)?;
        let from_game = self
            .game
            .allows_udp(&self.region, std::net::SocketAddr::V4(udp.src));
        if from_game && *udp.dst.ip() == self.adapter {
            Ok(())
        } else {
            Err(Stray::InboundNotGame)
        }
    }
}

/// Dropped packets by reason, for the UI warning and the log.
#[derive(Debug, Default)]
pub struct StrayCounts([AtomicU64; 7]);

impl StrayCounts {
    pub fn count(&self, stray: Stray) {
        self.0[Self::index(stray)].fetch_add(1, Ordering::Relaxed);
    }

    pub fn get(&self, stray: Stray) -> u64 {
        self.0[Self::index(stray)].load(Ordering::Relaxed)
    }

    pub fn total(&self) -> u64 {
        self.0.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }

    /// Dropped game traffic (what the player should be told about).
    pub fn affecting_game(&self) -> u64 {
        Stray::ALL
            .iter()
            .filter(|s| s.affects_game())
            .map(|&s| self.get(s))
            .sum()
    }

    /// Reasons with at least one drop.
    pub fn summary(&self) -> Vec<(Stray, u64)> {
        Stray::ALL
            .iter()
            .map(|&s| (s, self.get(s)))
            .filter(|&(_, n)| n > 0)
            .collect()
    }

    fn index(stray: Stray) -> usize {
        Stray::ALL
            .iter()
            .position(|&s| s == stray)
            .expect("listed in ALL")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingpath_games::SupportStatus;

    const ADAPTER: Ipv4Addr = Ipv4Addr::new(192, 0, 0, 8);

    fn filter() -> PacketFilter {
        let mut game = pingpath_games::embedded()
            .unwrap()
            .into_iter()
            .find(|g| g.id == "counter-strike-2")
            .unwrap();
        game.status = SupportStatus::Testing;
        PacketFilter::new(game, "eu-central", ADAPTER)
    }

    fn udp(src: &str, dst: &str) -> Vec<u8> {
        packet::build_udp(src.parse().unwrap(), dst.parse().unwrap(), b"x")
    }

    #[test]
    fn carries_only_game_udp_to_the_region() {
        let filter = filter();
        let out = |dst| udp("192.0.0.8:50000", dst);
        assert_eq!(filter.check(&out("203.0.113.5:27015")), Ok(()));
        assert_eq!(
            filter.check(&out("203.0.113.5:443")),
            Err(Stray::NotGamePort)
        );
        // eu-north's range, or multicast chatter: not this game's region.
        assert_eq!(
            filter.check(&out("203.0.113.70:27015")),
            Err(Stray::NotGameDestination)
        );
        assert_eq!(
            filter.check(&out("239.255.255.250:1900")),
            Err(Stray::NotGameDestination)
        );
        let mut tcp = out("203.0.113.5:27015");
        tcp[9] = packet::PROTOCOL_TCP;
        assert_eq!(filter.check(&tcp), Err(Stray::NotUdp));
        let mut v6 = vec![0u8; 48];
        v6[0] = 0x60;
        v6[24] = 0xff; // ff02::... multicast
        assert_eq!(filter.check(&v6), Err(Stray::NotGameDestination));
        assert_eq!(filter.check(&[0x45, 0]), Err(Stray::Malformed));
        assert!(Stray::NotUdp.affects_game() && !Stray::NotGameDestination.affects_game());
    }

    #[test]
    fn lets_in_only_game_replies_to_the_adapter() {
        let filter = filter();
        assert_eq!(
            filter.check_inbound(&udp("203.0.113.5:27015", "192.0.0.8:50000")),
            Ok(())
        );
        // Another source, another port, or another destination stays out.
        for (src, dst) in [
            ("198.51.100.1:27015", "192.0.0.8:50000"),
            ("203.0.113.5:445", "192.0.0.8:50000"),
            ("203.0.113.5:27015", "192.168.1.10:50000"),
        ] {
            assert_eq!(
                filter.check_inbound(&udp(src, dst)),
                Err(Stray::InboundNotGame),
                "{src} -> {dst}"
            );
        }
    }

    #[test]
    fn counts_by_reason() {
        let counts = StrayCounts::default();
        counts.count(Stray::NotUdp);
        counts.count(Stray::NotUdp);
        counts.count(Stray::NotGameDestination);
        assert_eq!(counts.total(), 3);
        assert_eq!(counts.affecting_game(), 2);
        assert_eq!(
            counts.summary(),
            vec![(Stray::NotGameDestination, 1), (Stray::NotUdp, 2)]
        );
    }
}
