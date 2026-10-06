//! The Windows backend: the PingPath adapter on Wintun (rule 4: the
//! signed Wintun driver and nothing else) and the route table through IP
//! Helper (docs/decisions/007 and 008).
//!
//! - **Adapter.** `PingPath`, created by this process with a fixed GUID,
//!   address `config.address`/32, the tunnel MTU, a high interface metric
//!   and no IPv6 routes. Dropping it removes it, and Windows removes every
//!   route through it, so a crashed service leaves game traffic going
//!   direct (fail open).
//! - **Game routes** are on-link routes on the adapter, IPv4 only, and
//!   never on another interface: being on the adapter is what makes a
//!   route ours.
//! - **Relay host routes** are /32 (or /128) routes through the physical
//!   gateway with metric [`HOST_ROUTE_METRIC`], recorded in an admin-only
//!   ledger before they are added; cleanup deletes only routes that match
//!   a ledger record exactly ([`crate::net::route_table`]).
//! - **Tunnel sockets** are pinned to the physical interface.
//!
//! Every adapter creation and removal and every route change is logged
//! (PLAN.md section 9). Relay and game server addresses are logged as the
//! route prefixes they are; the player's gateway goes through `Redacted`,
//! and the player's own addresses are never logged.
//!
//! # Why the `wintun` crate
//!
//! Of the two maintained bindings, `wintun` 0.5.1 builds warning-free on
//! the MSRV (1.85) and on current stable, and cargo reports no future
//! incompatibility for it. `wintun-bindings` 0.7.33 and later depend on
//! libloading 0.9, which needs Rust 1.88, so they can't build on our MSRV;
//! 0.7.32 and earlier declare the same `extern "stdcall"` functions as
//! `wintun` and bring a third windows-sys version. The catch with `wintun`:
//! `stdcall` on x86-64 is being phased out by rustc (rust-lang/rust#137018,
//! not yet reported for dependencies). When it becomes an error, or the
//! MSRV reaches 1.88, move to `wintun-bindings` (which declares
//! `extern "system"` and uses our windows-sys); its API is close to this
//! one. The crate's helpers that run `netsh` (address, MTU, DNS) are not
//! used: IP Helper does all of that here.

use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pingpath_diagnostics::Redacted;

use super::route_table::{self, HOST_ROUTE_METRIC, Ledger, RouteRow};
use super::{
    Adapter, AdapterConfig, CleanupReport, InterfaceId, NextHop, PacketChannels, Platform, Route,
};

mod adapter;
mod dll;
mod iphlp;
mod socket;

use adapter::{ADAPTER_GUID, Current, lock};

/// The Windows backend. The service creates one at start and shares it
/// with the engine and its guard thread.
pub struct WindowsPlatform {
    wintun: wintun::Wintun,
    ledger: Mutex<Ledger>,
    current: Current,
}

impl WindowsPlatform {
    /// Loads `wintun_dll` (an absolute path; the installer puts it next to
    /// the service) and opens the relay host route ledger at `ledger`
    /// (`%ProgramData%\PingPath\state\routes.json`, admin-only). A missing
    /// DLL is an error that says to reinstall.
    pub fn new(wintun_dll: &Path, ledger: PathBuf) -> io::Result<Self> {
        let wintun = dll::load(wintun_dll)?;
        tracing::info!(path = %wintun_dll.display(), "loaded wintun.dll");
        let ledger = match Ledger::load(ledger.clone()) {
            Ok(loaded) => {
                if !loaded.records().is_empty() {
                    tracing::info!(
                        routes = loaded.records().len(),
                        "the route ledger lists relay host routes from an earlier run"
                    );
                }
                loaded
            }
            Err(e) => {
                // Without the list, host routes an earlier run left stay in
                // place. That is harmless: they send relays the way they go
                // anyway. The file is replaced at the next save.
                tracing::warn!(path = %ledger.display(), error = %e, "the route ledger is unreadable; starting a new one");
                Ledger::new(ledger)
            }
        };
        Ok(Self {
            wintun,
            ledger: Mutex::new(ledger),
            current: Arc::new(Mutex::new(None)),
        })
    }

    /// The LUIDs of PingPath adapters: the one this process created, and
    /// one left by a crash if Windows still lists it.
    fn our_adapters(&self) -> Vec<u64> {
        let mut ours: Vec<u64> = lock(&self.current).iter().copied().collect();
        if let Ok(Some(luid)) = iphlp::guid_to_luid(ADAPTER_GUID) {
            if !ours.contains(&luid) {
                ours.push(luid);
            }
        }
        ours
    }

    fn add_host_route(&self, row: &RouteRow) -> io::Result<()> {
        let mut ledger = lock(&self.ledger);
        // Recorded before the route exists, so a crash right after the
        // add still leaves a record for cleanup.
        ledger.insert(*row)?;
        match iphlp::create_route(row) {
            Ok(true) => {
                tracing::info!(
                    destination = %row.destination,
                    gateway = %gateway(row),
                    interface = format_args!("{:#x}", row.interface),
                    metric = row.metric,
                    "relay host route added"
                );
                Ok(())
            }
            Ok(false) => {
                // Windows names a route by interface, destination and next
                // hop, so one that differs only in its metric is someone
                // else's: it already sends the relay the right way, and we
                // must never delete it.
                let existing = iphlp::get_route(row)?;
                if existing.map(|r| r.metric) == Some(HOST_ROUTE_METRIC) {
                    tracing::info!(destination = %row.destination, "relay host route already present");
                } else {
                    ledger.remove(row)?;
                    tracing::info!(
                        destination = %row.destination,
                        metric = existing.map(|r| r.metric),
                        "a route for this relay already exists and isn't ours; using it and leaving it alone"
                    );
                }
                Ok(())
            }
            Err(e) => {
                if let Err(forget) = ledger.remove(row) {
                    tracing::warn!(error = %forget, "could not update the route ledger");
                }
                Err(e)
            }
        }
    }

    fn remove_host_route(&self, row: &RouteRow) -> io::Result<()> {
        let mut ledger = lock(&self.ledger);
        match iphlp::get_route(row)? {
            Some(existing) if ledger.owns(&existing) => {
                if iphlp::delete_route(row)? {
                    tracing::info!(
                        destination = %row.destination,
                        gateway = %gateway(row),
                        interface = format_args!("{:#x}", row.interface),
                        "relay host route removed"
                    );
                }
            }
            Some(existing) => tracing::info!(
                destination = %row.destination,
                metric = existing.metric,
                "left a route for this relay that isn't ours"
            ),
            None => {
                tracing::debug!(destination = %row.destination, "relay host route already gone")
            }
        }
        ledger.remove(row)
    }

    /// Deletes the relay host routes an earlier run left: exactly the
    /// table rows that match a ledger record in every field. Records whose
    /// route is gone, or no longer matches, are forgotten; a record whose
    /// delete failed is kept for the next cleanup.
    fn remove_leftover_host_routes(&self) -> io::Result<usize> {
        let mut ledger = lock(&self.ledger);
        if ledger.records().is_empty() {
            return Ok(0);
        }
        let table = iphlp::route_table(iphlp::ANY_FAMILY)?;
        let mut removed = 0;
        let mut failed = Vec::new();
        for row in ledger.owned_rows(&table) {
            match iphlp::delete_route(&row) {
                Ok(true) => {
                    removed += 1;
                    tracing::info!(
                        destination = %row.destination,
                        gateway = %gateway(&row),
                        interface = format_args!("{:#x}", row.interface),
                        "removed a relay host route left by an earlier run"
                    );
                }
                // It went between the listing and the delete.
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(destination = %row.destination, error = %e, "could not remove a leftover relay host route");
                    failed.push(row);
                }
            }
        }
        ledger.retain(|record| failed.contains(record))?;
        Ok(removed)
    }
}

/// The next hop for a log line: the player's gateway can be a public
/// address that locates them, so it is shortened.
fn gateway(row: &RouteRow) -> String {
    match row.next_hop {
        Some(hop) => Redacted(hop).to_string(),
        None => "on-link".into(),
    }
}

impl Platform for WindowsPlatform {
    fn create_adapter(
        &self,
        config: &AdapterConfig,
    ) -> io::Result<(Box<dyn Adapter>, PacketChannels)> {
        let (adapter, channels) = adapter::create(&self.wintun, config, &self.current)?;
        Ok((Box::new(adapter), channels))
    }

    /// How the physical network reaches `dst`, with PingPath's own routes
    /// set aside.
    ///
    /// When none of our routes covers `dst`, the system's own answer
    /// (`GetBestRoute2`) is the physical route. When one does (the relay's
    /// host route, or a game range on the adapter that happens to include
    /// it), asking the system would only return our route, which still
    /// points where the network was when we added it. Then the route table
    /// is read and the choice redone the way the IP stack makes it, without
    /// our rows: longest prefix, then lowest route plus interface metric,
    /// among connected interfaces. That is how a new gateway or a move
    /// from Wi-Fi to cable becomes visible while a session runs.
    fn best_route(&self, dst: IpAddr) -> io::Result<NextHop> {
        let family = iphlp::family_of(dst);
        let table = iphlp::route_table(family)?;
        let ours = self.our_adapters();
        let system = if route_table::covered_by_ours(&table, &ours, dst) {
            None
        } else {
            Some(iphlp::best_route(dst)?).filter(|row| !route_table::is_ours(row, &ours))
        };
        let row = match system {
            Some(row) => row,
            None => {
                let interfaces = iphlp::interface_table(family)?;
                route_table::physical_route(&table, &interfaces, &ours, dst).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NetworkUnreachable,
                        format!("no physical route to {}", Redacted(dst)),
                    )
                })?
            }
        };
        Ok(NextHop {
            gateway: row.next_hop,
            interface: InterfaceId(row.interface),
        })
    }

    fn add_route(&self, route: &Route) -> io::Result<()> {
        let row = RouteRow::for_route(route)?;
        match route {
            Route::Adapter { .. } => {
                // Game routes are recognised by their interface alone, so
                // they go on the PingPath adapter and nowhere else.
                if !self.our_adapters().contains(&row.interface) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "interface {:#x} is not the PingPath adapter; game routes go only there",
                            row.interface
                        ),
                    ));
                }
                let added = iphlp::create_route(&row)?;
                tracing::info!(
                    destination = %row.destination,
                    interface = format_args!("{:#x}", row.interface),
                    already_present = !added,
                    "game route added to the adapter"
                );
                Ok(())
            }
            Route::Host { .. } => self.add_host_route(&row),
        }
    }

    fn remove_route(&self, route: &Route) -> io::Result<()> {
        let row = RouteRow::for_route(route)?;
        match route {
            // Anything on the adapter is ours, and nothing elsewhere is a
            // game route of ours: a route on another interface is left
            // alone, and one on an adapter that is gone went with it.
            Route::Adapter { .. } if !self.our_adapters().contains(&row.interface) => {
                if iphlp::interface_gone(row.interface) {
                    tracing::debug!(destination = %row.destination, "game route already gone with its adapter");
                } else {
                    tracing::warn!(
                        destination = %row.destination,
                        interface = format_args!("{:#x}", row.interface),
                        "not removing a route on an interface that isn't the PingPath adapter"
                    );
                }
                Ok(())
            }
            Route::Adapter { .. } => {
                if iphlp::delete_route(&row)? {
                    tracing::info!(
                        destination = %row.destination,
                        interface = format_args!("{:#x}", row.interface),
                        "game route removed from the adapter"
                    );
                } else {
                    tracing::debug!(destination = %row.destination, "game route already gone");
                }
                Ok(())
            }
            Route::Host { .. } => self.remove_host_route(&row),
        }
    }

    /// Removes what an earlier run left: relay host routes that match the
    /// ledger exactly, and a stale adapter.
    ///
    /// Game routes need nothing here: they live on the adapter and went
    /// with it. A crashed service's adapter normally goes too (on Windows
    /// 10 and later Wintun's device ends with the process that created
    /// it); if Windows still lists one with our GUID, Wintun treats it as
    /// orphaned, and creating an adapter removes orphans. So a stale
    /// adapter is replaced and dropped here, and is otherwise removed the
    /// next time the adapter is created.
    fn cleanup(&self) -> io::Result<CleanupReport> {
        let mut report = CleanupReport::default();
        match self.remove_leftover_host_routes() {
            Ok(removed) => report.routes = removed,
            Err(e) => tracing::warn!(error = %e, "could not check for leftover relay host routes"),
        }
        let ours = *lock(&self.current);
        if ours.is_none() {
            if let Some(stale) = iphlp::guid_to_luid(ADAPTER_GUID)? {
                tracing::warn!(
                    interface = format_args!("{stale:#x}"),
                    "a PingPath adapter from an earlier run is still present; removing it"
                );
                adapter::replace_stale(&self.wintun)?;
                if iphlp::guid_to_luid(ADAPTER_GUID)?.is_none() {
                    report.adapters += 1;
                    tracing::info!(interface = format_args!("{stale:#x}"), "adapter removed");
                } else {
                    tracing::warn!("the stale PingPath adapter is still listed");
                }
            }
        }
        if report != CleanupReport::default() {
            tracing::info!(
                adapters = report.adapters,
                routes = report.routes,
                "cleanup removed leftovers"
            );
        }
        Ok(report)
    }

    /// Pins the tunnel socket to the physical interface `via` uses
    /// (`IP_UNICAST_IF`) and turns off `SIO_UDP_CONNRESET`. Both are
    /// attempted; the first failure is returned.
    ///
    /// The engine passes one `via` (the first relay's) for both of its
    /// sockets. A socket of the other address family than `via`'s gateway
    /// is left unpinned: that interface was chosen for the other family
    /// and may not carry this one. The relay host routes still keep its
    /// traffic off the adapter, and IPv6 never has a route there anyway.
    fn prepare_socket(&self, socket: &tokio::net::UdpSocket, via: NextHop) -> io::Result<()> {
        let resets = socket::ignore_connection_resets(socket);
        let pinned = iphlp::luid_to_index(via.interface.0).and_then(|index| {
            let ipv6 = socket.local_addr()?.is_ipv6();
            if via.gateway.is_some_and(|gateway| gateway.is_ipv6() != ipv6) {
                return Ok(());
            }
            socket::pin_to_interface(socket, index, ipv6)?;
            tracing::debug!(
                interface = format_args!("{:#x}", via.interface.0),
                index,
                ipv6,
                "tunnel socket pinned to the physical interface"
            );
            Ok(())
        });
        resets.and(pinned)
    }
}

/// Every route in the system table, both families: for diagnostics and
/// the Windows tests.
pub fn routes() -> io::Result<Vec<RouteRow>> {
    iphlp::route_table(iphlp::ANY_FAMILY)
}

/// The PingPath adapter Windows lists now, created by this process or
/// left by another, if any.
pub fn find_adapter() -> io::Result<Option<InterfaceId>> {
    Ok(iphlp::guid_to_luid(ADAPTER_GUID)?.map(InterfaceId))
}

/// The interface index of a LUID, for tools that take an index.
pub fn interface_index(interface: InterfaceId) -> io::Result<u32> {
    iphlp::luid_to_index(interface.0)
}
