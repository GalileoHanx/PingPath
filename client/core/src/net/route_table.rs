//! The Windows backend's route table decisions, kept free of system calls
//! so they are tested on every OS: which rows the backend adds for a
//! [`Route`], which existing rows are PingPath's, which physical route a
//! destination takes when PingPath's own routes are set aside, and the
//! ledger of relay host routes that crash cleanup works from.
//!
//! How PingPath recognises its own routes (docs/decisions/008), never by
//! the route's protocol value, which `route add` and `New-NetRoute` share:
//!
//! - Game routes live on the PingPath adapter. Anything on the adapter is
//!   ours, and Windows removes those routes when the adapter goes away,
//!   including when the service crashes.
//! - Relay host routes live on the physical interface, so they survive a
//!   crash. Each carries the metric [`HOST_ROUTE_METRIC`] and is written to
//!   the [`Ledger`] before it is added. Cleanup deletes a route only when it
//!   matches a ledger record exactly: destination, next hop, interface and
//!   metric. A route someone else added is never touched, even when it
//!   goes to the same place.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

use super::Route;

/// The route metric of relay host routes: a /32 always wins over shorter
/// prefixes whatever its metric, so the value only serves to recognise
/// our rows. Uncommon, and below the 9999 that `route.exe` accepts.
pub const HOST_ROUTE_METRIC: u32 = 293;

/// The route metric of game routes on the adapter. Windows adds the
/// adapter's (high) interface metric; longest-prefix match is what sends
/// game traffic to the adapter.
pub const ADAPTER_ROUTE_METRIC: u32 = 0;

/// One row of the system route table, as the backend reads and writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RouteRow {
    /// The interface's LUID.
    pub interface: u64,
    pub destination: IpNet,
    /// `None` for an on-link route (next hop `0.0.0.0` or `::`).
    pub next_hop: Option<IpAddr>,
    /// The route's own metric; Windows adds the interface metric to it.
    pub metric: u32,
}

impl RouteRow {
    /// The row the backend adds for `route`.
    ///
    /// Game routes are on-link through the adapter (Wintun is a layer-3
    /// link, so there is no gateway) and IPv4 only: IPv6 is never routed
    /// to the adapter, because exits don't carry it. Relay host routes are
    /// /32 or /128 through the gateway and interface the relay is reached
    /// by now.
    pub fn for_route(route: &Route) -> io::Result<RouteRow> {
        match *route {
            Route::Adapter { prefix, interface } => {
                if matches!(prefix, IpNet::V6(_)) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{prefix}: IPv6 is never routed to the PingPath adapter"),
                    ));
                }
                if prefix.prefix_len() == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "a default route is never routed to the PingPath adapter",
                    ));
                }
                Ok(RouteRow {
                    interface: interface.0,
                    destination: prefix.trunc(),
                    next_hop: None,
                    metric: ADAPTER_ROUTE_METRIC,
                })
            }
            Route::Host { address, via } => {
                if let Some(gateway) = via.gateway {
                    if gateway.is_ipv4() != address.is_ipv4() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "host route to {address} through a gateway of the other address family"
                            ),
                        ));
                    }
                }
                Ok(RouteRow {
                    interface: via.interface.0,
                    destination: IpNet::from(address),
                    next_hop: via.gateway.filter(|gateway| !gateway.is_unspecified()),
                    metric: HOST_ROUTE_METRIC,
                })
            }
        }
    }

    /// A /32 or /128.
    pub fn is_host(&self) -> bool {
        self.destination.prefix_len() == self.destination.max_prefix_len()
    }

    /// Windows identifies a route by its interface, destination and next
    /// hop; the metric is a setting of the route, not part of its name.
    pub fn same_route(&self, other: &RouteRow) -> bool {
        self.interface == other.interface
            && self.destination == other.destination
            && self.next_hop == other.next_hop
    }

    /// The unspecified address of the destination's family: the next hop
    /// Windows expects for an on-link route.
    pub fn on_link_next_hop(&self) -> IpAddr {
        match self.destination {
            IpNet::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpNet::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        }
    }
}

/// What the route choice needs to know about an interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterfaceState {
    /// The interface metric, added to each route's own metric.
    pub metric: u32,
    /// Media connected: Windows doesn't use routes through an interface
    /// that isn't.
    pub connected: bool,
}

/// Whether `row` is one of PingPath's: on one of our adapters (`adapters`
/// holds their LUIDs: the one this process created, and a stale one left
/// by a crash), or a host route with our metric.
pub fn is_ours(row: &RouteRow, adapters: &[u64]) -> bool {
    adapters.contains(&row.interface) || (row.is_host() && row.metric == HOST_ROUTE_METRIC)
}

/// Whether one of PingPath's routes covers `dst`, so the system's own
/// answer for `dst` would be our route rather than the physical one.
pub fn covered_by_ours(table: &[RouteRow], adapters: &[u64], dst: IpAddr) -> bool {
    table
        .iter()
        .any(|row| row.destination.contains(&dst) && is_ours(row, adapters))
}

/// The route Windows would use for `dst` if PingPath's routes weren't
/// there: the longest matching prefix, then the lowest route metric plus
/// interface metric, among interfaces that are connected. This is how the
/// IP stack itself chooses, minus our rows, so a network change (a new
/// gateway, Wi-Fi to cable) shows up even while a relay host route still
/// points at the old one.
pub fn physical_route(
    table: &[RouteRow],
    interfaces: &HashMap<u64, InterfaceState>,
    adapters: &[u64],
    dst: IpAddr,
) -> Option<RouteRow> {
    table
        .iter()
        .filter(|row| row.destination.contains(&dst) && !is_ours(row, adapters))
        .filter_map(|row| {
            let interface = interfaces.get(&row.interface)?;
            interface
                .connected
                .then(|| (row, row.metric.saturating_add(interface.metric)))
        })
        .min_by_key(|(row, metric)| (std::cmp::Reverse(row.destination.prefix_len()), *metric))
        .map(|(row, _)| *row)
}

/// The relay host routes PingPath has added and not yet removed, kept in
/// an admin-only file (`%ProgramData%\PingPath\state\routes.json`) so a
/// later run can remove what a crash left behind. A record is written
/// before its route is added and deleted after the route is removed, and
/// every write replaces the file atomically (a temporary file, then a
/// rename), so a crash mid-write leaves the old list or the new one.
#[derive(Debug)]
pub struct Ledger {
    path: PathBuf,
    routes: Vec<RouteRow>,
}

#[derive(Serialize, Deserialize)]
struct LedgerFile {
    routes: Vec<RouteRow>,
}

impl Ledger {
    /// An empty ledger that will be saved at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            routes: Vec::new(),
        }
    }

    /// Reads the ledger at `path`; a missing file is an empty ledger.
    pub fn load(path: PathBuf) -> io::Result<Self> {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::new(path)),
            Err(e) => return Err(e),
        };
        let file: LedgerFile = serde_json::from_str(&text).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {e}", path.display()),
            )
        })?;
        Ok(Self {
            path,
            routes: file.routes,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn records(&self) -> &[RouteRow] {
        &self.routes
    }

    pub fn contains(&self, row: &RouteRow) -> bool {
        self.routes.contains(row)
    }

    /// Records `row` and saves the ledger.
    pub fn insert(&mut self, row: RouteRow) -> io::Result<()> {
        if self.routes.contains(&row) {
            return Ok(());
        }
        self.routes.push(row);
        self.save()
    }

    /// Forgets `row` and saves the ledger.
    pub fn remove(&mut self, row: &RouteRow) -> io::Result<()> {
        let before = self.routes.len();
        self.routes.retain(|r| r != row);
        if self.routes.len() == before {
            return Ok(());
        }
        self.save()
    }

    /// Keeps only the records `keep` accepts and saves the ledger.
    pub fn retain(&mut self, keep: impl FnMut(&RouteRow) -> bool) -> io::Result<()> {
        let before = self.routes.len();
        self.routes.retain(keep);
        if self.routes.len() == before {
            return Ok(());
        }
        self.save()
    }

    /// Whether `row` is a relay host route this ledger owns: a host route
    /// with our metric that matches a record in every field.
    pub fn owns(&self, row: &RouteRow) -> bool {
        row.is_host() && row.metric == HOST_ROUTE_METRIC && self.routes.contains(row)
    }

    /// The rows of `table` the ledger owns: the only ones cleanup deletes.
    pub fn owned_rows(&self, table: &[RouteRow]) -> Vec<RouteRow> {
        table.iter().filter(|row| self.owns(row)).copied().collect()
    }

    /// Replaces the file atomically: the new list goes to a temporary file
    /// next to it, is flushed to disk, then renamed over the old one.
    pub fn save(&self) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(&LedgerFile {
            routes: self.routes.clone(),
        })
        .map_err(io::Error::other)?;
        let mut temporary = self.path.clone().into_os_string();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        {
            let mut file = fs::File::create(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&temporary, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{InterfaceId, NextHop};

    const ETHERNET: u64 = 0x0006_0000_0100_0000;
    const WIFI: u64 = 0x0047_0000_0200_0000;
    const ADAPTER: u64 = 0x0035_0000_0300_0000;

    fn row(interface: u64, destination: &str, next_hop: Option<&str>, metric: u32) -> RouteRow {
        RouteRow {
            interface,
            destination: destination.parse().unwrap(),
            next_hop: next_hop.map(|hop| hop.parse().unwrap()),
            metric,
        }
    }

    fn host(address: &str, gateway: Option<&str>, interface: u64) -> Route {
        Route::Host {
            address: address.parse().unwrap(),
            via: NextHop {
                gateway: gateway.map(|g| g.parse().unwrap()),
                interface: InterfaceId(interface),
            },
        }
    }

    #[test]
    fn game_routes_are_on_link_ipv4_prefixes_on_the_adapter() {
        let route = Route::Adapter {
            prefix: "203.0.113.77/24".parse().unwrap(),
            interface: InterfaceId(ADAPTER),
        };
        assert_eq!(
            RouteRow::for_route(&route).unwrap(),
            row(ADAPTER, "203.0.113.0/24", None, ADAPTER_ROUTE_METRIC)
        );
        assert_eq!(
            RouteRow::for_route(&route).unwrap().on_link_next_hop(),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
    }

    #[test]
    fn ipv6_and_default_routes_never_go_to_the_adapter() {
        for prefix in ["2001:db8::/32", "0.0.0.0/0"] {
            let route = Route::Adapter {
                prefix: prefix.parse().unwrap(),
                interface: InterfaceId(ADAPTER),
            };
            let error = RouteRow::for_route(&route).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{prefix}");
        }
    }

    #[test]
    fn host_routes_are_host_prefixes_with_the_marker_metric() {
        let v4 =
            RouteRow::for_route(&host("198.51.100.10", Some("192.168.1.1"), ETHERNET)).unwrap();
        assert_eq!(
            v4,
            row(
                ETHERNET,
                "198.51.100.10/32",
                Some("192.168.1.1"),
                HOST_ROUTE_METRIC
            )
        );
        assert!(v4.is_host());

        let v6 = RouteRow::for_route(&host("2001:db8::10", Some("fe80::1"), WIFI)).unwrap();
        assert_eq!(v6.destination, "2001:db8::10/128".parse::<IpNet>().unwrap());
        assert!(v6.is_host());

        // A relay on the same LAN is reached on-link, without a gateway;
        // an unspecified gateway means the same.
        let lan = RouteRow::for_route(&host("192.168.1.20", None, ETHERNET)).unwrap();
        assert_eq!(lan.next_hop, None);
        let unspecified = RouteRow::for_route(&host("192.168.1.20", Some("0.0.0.0"), ETHERNET));
        assert_eq!(unspecified.unwrap().next_hop, None);

        let mixed = RouteRow::for_route(&host("198.51.100.10", Some("fe80::1"), ETHERNET));
        assert_eq!(mixed.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn windows_names_a_route_without_its_metric() {
        let ours = row(
            ETHERNET,
            "198.51.100.10/32",
            Some("192.168.1.1"),
            HOST_ROUTE_METRIC,
        );
        let foreign = RouteRow { metric: 25, ..ours };
        assert!(ours.same_route(&foreign));
        assert!(!ours.same_route(&RouteRow {
            interface: WIFI,
            ..ours
        }));
        assert!(!ours.same_route(&RouteRow {
            next_hop: None,
            ..ours
        }));
    }

    #[test]
    fn the_ledger_owns_only_exact_matches() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::new(dir.path().join("routes.json"));
        let record = row(
            ETHERNET,
            "198.51.100.10/32",
            Some("192.168.1.1"),
            HOST_ROUTE_METRIC,
        );
        ledger.insert(record).unwrap();

        assert!(ledger.owns(&record));
        // The same destination with anything else different is someone
        // else's route, or ours from a network we have since left.
        for other in [
            RouteRow {
                metric: 25,
                ..record
            },
            RouteRow {
                next_hop: Some("192.168.1.254".parse().unwrap()),
                ..record
            },
            RouteRow {
                next_hop: None,
                ..record
            },
            RouteRow {
                interface: WIFI,
                ..record
            },
            RouteRow {
                destination: "198.51.100.11/32".parse().unwrap(),
                ..record
            },
        ] {
            assert!(!ledger.owns(&other), "{other:?}");
        }

        // A record that isn't a host route with our metric owns nothing,
        // whatever the file says.
        let odd = row(
            ETHERNET,
            "198.51.100.0/24",
            Some("192.168.1.1"),
            HOST_ROUTE_METRIC,
        );
        let unmarked = row(ETHERNET, "198.51.100.12/32", Some("192.168.1.1"), 10);
        ledger.insert(odd).unwrap();
        ledger.insert(unmarked).unwrap();
        assert!(!ledger.owns(&odd));
        assert!(!ledger.owns(&unmarked));

        let table = vec![
            record,
            RouteRow {
                metric: 25,
                ..record
            },
            odd,
            unmarked,
            row(ETHERNET, "0.0.0.0/0", Some("192.168.1.1"), 0),
        ];
        assert_eq!(ledger.owned_rows(&table), vec![record]);
    }

    #[test]
    fn the_ledger_survives_a_restart_and_saves_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("routes.json");
        assert!(Ledger::load(path.clone()).unwrap().records().is_empty());

        let a = row(
            ETHERNET,
            "198.51.100.10/32",
            Some("192.168.1.1"),
            HOST_ROUTE_METRIC,
        );
        let b = row(WIFI, "2001:db8::10/128", None, HOST_ROUTE_METRIC);
        let mut ledger = Ledger::new(path.clone());
        ledger.insert(a).unwrap();
        ledger.insert(b).unwrap();
        ledger.insert(a).unwrap();
        assert_eq!(Ledger::load(path.clone()).unwrap().records(), &[a, b]);
        assert!(!dir.path().join("state").join("routes.json.tmp").exists());

        ledger.remove(&a).unwrap();
        assert_eq!(Ledger::load(path.clone()).unwrap().records(), &[b]);
        ledger.retain(|_| false).unwrap();
        assert!(Ledger::load(path.clone()).unwrap().records().is_empty());

        fs::write(&path, "not json").unwrap();
        assert_eq!(
            Ledger::load(path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    fn interfaces(wifi_connected: bool) -> HashMap<u64, InterfaceState> {
        HashMap::from([
            (
                ETHERNET,
                InterfaceState {
                    metric: 25,
                    connected: true,
                },
            ),
            (
                WIFI,
                InterfaceState {
                    metric: 50,
                    connected: wifi_connected,
                },
            ),
            (
                ADAPTER,
                InterfaceState {
                    metric: 9000,
                    connected: true,
                },
            ),
        ])
    }

    #[test]
    fn the_physical_route_ignores_ours_and_follows_the_network() {
        let relay: IpAddr = "198.51.100.10".parse().unwrap();
        let table = vec![
            row(ETHERNET, "0.0.0.0/0", Some("192.168.1.1"), 0),
            row(WIFI, "0.0.0.0/0", Some("10.0.0.1"), 0),
            // Ours: a game range on the adapter that covers the relay, and
            // the relay's host route through the network we were on before.
            row(ADAPTER, "198.51.100.0/24", None, ADAPTER_ROUTE_METRIC),
            row(
                WIFI,
                "198.51.100.10/32",
                Some("10.0.0.1"),
                HOST_ROUTE_METRIC,
            ),
            // Not ours and not covering the relay.
            row(ETHERNET, "192.168.1.0/24", None, 256),
        ];
        assert!(covered_by_ours(&table, &[ADAPTER], relay));

        // Both default routes match; Ethernet's total metric is lower.
        let chosen = physical_route(&table, &interfaces(true), &[ADAPTER], relay).unwrap();
        assert_eq!(chosen, table[0]);

        // A longer prefix wins over a lower metric.
        let mut longer = table.clone();
        longer.push(row(WIFI, "198.51.0.0/16", Some("10.0.0.1"), 100));
        let chosen = physical_route(&longer, &interfaces(true), &[ADAPTER], relay).unwrap();
        assert_eq!(chosen, longer[5]);

        // ...unless its interface is disconnected.
        let chosen = physical_route(&longer, &interfaces(false), &[ADAPTER], relay).unwrap();
        assert_eq!(chosen, longer[0]);

        // A stale adapter from a crash is ours too.
        let stale = 0x0035_0000_0400_0000;
        let mut with_stale = table.clone();
        with_stale.push(row(stale, "198.51.100.0/25", None, 0));
        let mut states = interfaces(true);
        states.insert(
            stale,
            InterfaceState {
                metric: 9000,
                connected: true,
            },
        );
        let chosen = physical_route(&with_stale, &states, &[ADAPTER, stale], relay).unwrap();
        assert_eq!(chosen, table[0]);

        // Nothing physical: no answer rather than our own route.
        let only_ours = vec![table[2], table[3]];
        assert_eq!(
            physical_route(&only_ours, &interfaces(true), &[ADAPTER], relay),
            None
        );
        // Interfaces the system doesn't list are not used.
        assert_eq!(
            physical_route(&table, &HashMap::new(), &[ADAPTER], relay),
            None
        );
    }

    #[test]
    fn only_our_rows_count_as_covering() {
        let dst: IpAddr = "203.0.113.5".parse().unwrap();
        let foreign = vec![
            row(ETHERNET, "0.0.0.0/0", Some("192.168.1.1"), 0),
            row(ETHERNET, "203.0.113.5/32", Some("192.168.1.1"), 10),
        ];
        assert!(!covered_by_ours(&foreign, &[ADAPTER], dst));
        let marked = vec![row(
            ETHERNET,
            "203.0.113.5/32",
            Some("192.168.1.1"),
            HOST_ROUTE_METRIC,
        )];
        assert!(covered_by_ours(&marked, &[ADAPTER], dst));
        let on_adapter = vec![row(ADAPTER, "203.0.0.0/16", None, 0)];
        assert!(covered_by_ours(&on_adapter, &[ADAPTER], dst));
        assert!(!covered_by_ours(&on_adapter, &[], dst));
    }
}
