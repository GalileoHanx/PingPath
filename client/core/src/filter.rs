//! Which packets that reach the adapter go into the tunnel
//! (docs/decisions/008-split-tunnel-routing.md): IPv4 UDP to the region's
//! ranges and the game's ports, the same check the exit makes. The rest is
//! dropped and counted, so a too-broad game definition shows up in the UI
//! and the diagnostic report instead of failing silently.

use std::sync::atomic::{AtomicU64, Ordering};

use pingpath_games::GameDefinition;
use pingpath_tunnel::packet::{self, PacketError};
use serde::Serialize;

/// Why a packet from the adapter wasn't carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stray {
    /// IPv6: exits don't forward it yet (ADR 005).
    Ipv6NotCarried,
    /// TCP, ICMP or another protocol: exits carry only the game's UDP.
    NotUdp,
    /// UDP to a port the definition doesn't list.
    NotGamePort,
    /// Outside the region's ranges (shouldn't happen: only those are routed).
    NotGameRange,
    Fragment,
    Malformed,
}

impl Stray {
    pub const ALL: [Stray; 6] = [
        Stray::Ipv6NotCarried,
        Stray::NotUdp,
        Stray::NotGamePort,
        Stray::NotGameRange,
        Stray::Fragment,
        Stray::Malformed,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Stray::Ipv6NotCarried => "ipv6_not_carried",
            Stray::NotUdp => "not_udp",
            Stray::NotGamePort => "not_game_port",
            Stray::NotGameRange => "not_game_range",
            Stray::Fragment => "fragment",
            Stray::Malformed => "malformed",
        }
    }
}

/// Decides, for one game and region, whether a packet goes into the tunnel.
#[derive(Debug, Clone)]
pub struct PacketFilter {
    game: GameDefinition,
    region: String,
}

impl PacketFilter {
    pub fn new(game: GameDefinition, region: &str) -> Self {
        Self {
            game,
            region: region.to_string(),
        }
    }

    /// `Ok(())` to carry the packet, or why not.
    pub fn check(&self, packet: &[u8]) -> Result<(), Stray> {
        let udp = packet::parse_udp(packet).map_err(|e| match e {
            PacketError::NotIpv4(6) => Stray::Ipv6NotCarried,
            PacketError::NotIpv4(_) | PacketError::Malformed => Stray::Malformed,
            PacketError::Fragment => Stray::Fragment,
            PacketError::NotUdp(_) => Stray::NotUdp,
        })?;
        let dst = std::net::SocketAddr::V4(udp.dst);
        if self.game.allows_udp(&self.region, dst) {
            return Ok(());
        }
        let in_range = self
            .game
            .region(&self.region)
            .is_some_and(|r| r.server_ranges.iter().any(|net| net.contains(&dst.ip())));
        Err(if in_range {
            Stray::NotGamePort
        } else {
            Stray::NotGameRange
        })
    }
}

/// Dropped packets by reason, for the UI warning and the log.
#[derive(Debug, Default)]
pub struct StrayCounts([AtomicU64; 6]);

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

    fn filter() -> PacketFilter {
        let mut game = pingpath_games::embedded()
            .unwrap()
            .into_iter()
            .find(|g| g.id == "counter-strike-2")
            .unwrap();
        game.status = SupportStatus::Testing;
        PacketFilter::new(game, "eu-central")
    }

    fn udp(dst: &str) -> Vec<u8> {
        packet::build_udp(
            "10.200.0.2:50000".parse().unwrap(),
            dst.parse().unwrap(),
            b"x",
        )
    }

    #[test]
    fn carries_only_game_udp_to_the_region() {
        let filter = filter();
        assert_eq!(filter.check(&udp("203.0.113.5:27015")), Ok(()));
        assert_eq!(
            filter.check(&udp("203.0.113.5:443")),
            Err(Stray::NotGamePort)
        );
        // eu-north's range: not this region.
        assert_eq!(
            filter.check(&udp("203.0.113.70:27015")),
            Err(Stray::NotGameRange)
        );
        let mut tcp = udp("203.0.113.5:27015");
        tcp[9] = packet::PROTOCOL_TCP;
        assert_eq!(filter.check(&tcp), Err(Stray::NotUdp));
        let mut v6 = vec![0u8; 48];
        v6[0] = 0x60;
        assert_eq!(filter.check(&v6), Err(Stray::Ipv6NotCarried));
        assert_eq!(filter.check(&[0x45, 0]), Err(Stray::Malformed));
    }

    #[test]
    fn counts_by_reason() {
        let counts = StrayCounts::default();
        counts.count(Stray::NotUdp);
        counts.count(Stray::NotUdp);
        counts.count(Stray::Ipv6NotCarried);
        assert_eq!(counts.total(), 3);
        assert_eq!(
            counts.summary(),
            vec![(Stray::Ipv6NotCarried, 1), (Stray::NotUdp, 2)]
        );
    }
}
