//! Which routes Optimize adds (docs/decisions/008-split-tunnel-routing.md):
//! host routes that pin every relay to the physical network, then the
//! selected game's selected region, and nothing else.

use std::net::IpAddr;

use ipnet::IpNet;
use pingpath_games::GameDefinition;

use crate::policy::{self, Refusal};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutePlan {
    /// Through the PingPath adapter: the region's server ranges, IPv4 and
    /// IPv6 (IPv6 is dropped at the adapter until exits carry it, so it
    /// never bypasses the session unnoticed).
    pub game_routes: Vec<IpNet>,
    /// Host routes through the current physical gateway, added first so
    /// tunnel traffic can never enter the tunnel.
    pub relay_hosts: Vec<IpAddr>,
}

/// The routes for `game` in `region`, with host routes for `relays`.
/// Refuses games whose status doesn't allow routing (rule 7).
pub fn plan(game: &GameDefinition, region: &str, relays: &[IpAddr]) -> Result<RoutePlan, Refusal> {
    policy::check(game, region)?;
    let region = game.region(region).expect("checked by policy");
    let mut game_routes: Vec<IpNet> = Vec::new();
    for net in &region.server_ranges {
        // Never a default route, whatever a definition says (definitions
        // are validated to /16 or narrower, but this is the last line).
        if net.prefix_len() == 0 {
            continue;
        }
        let net = net.trunc();
        if !game_routes.contains(&net) {
            game_routes.push(net);
        }
    }
    let mut relay_hosts: Vec<IpAddr> = Vec::new();
    for ip in relays {
        // A relay on this machine needs no route (development and tests).
        if !ip.is_loopback() && !ip.is_unspecified() && !relay_hosts.contains(ip) {
            relay_hosts.push(*ip);
        }
    }
    Ok(RoutePlan {
        game_routes,
        relay_hosts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingpath_games::SupportStatus;

    fn game(status: SupportStatus) -> GameDefinition {
        let mut game = pingpath_games::embedded()
            .unwrap()
            .into_iter()
            .find(|g| g.id == "counter-strike-2")
            .unwrap();
        game.status = status;
        game
    }

    #[test]
    fn routes_only_the_selected_region() {
        let game = game(SupportStatus::Testing);
        let relays: Vec<IpAddr> = vec![
            "198.51.100.10".parse().unwrap(),
            "198.51.100.10".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
        ];
        let plan = plan(&game, "eu-central", &relays).unwrap();
        assert_eq!(
            plan.game_routes,
            game.region("eu-central").unwrap().server_ranges
        );
        for other in &game.region("eu-north").unwrap().server_ranges {
            assert!(!plan.game_routes.contains(other));
        }
        assert_eq!(
            plan.relay_hosts,
            vec!["198.51.100.10".parse::<IpAddr>().unwrap()]
        );
    }

    #[test]
    fn never_routes_untested_or_unsupported_games() {
        for status in [SupportStatus::Untested, SupportStatus::Unsupported] {
            assert!(matches!(
                plan(&game(status), "eu-central", &[]),
                Err(Refusal::NotRoutable { .. })
            ));
        }
        assert!(matches!(
            plan(&game(SupportStatus::Supported), "mars", &[]),
            Err(Refusal::UnknownRegion { .. })
        ));
    }

    #[test]
    fn never_adds_a_default_route() {
        let mut game = game(SupportStatus::Supported);
        game.regions[0].server_ranges = vec![
            "0.0.0.0/0".parse().unwrap(),
            "203.0.113.0/26".parse().unwrap(),
        ];
        let plan = plan(&game, "eu-central", &[]).unwrap();
        assert_eq!(
            plan.game_routes,
            vec!["203.0.113.0/26".parse::<IpNet>().unwrap()]
        );
    }
}
