//! What the engine needs from the operating system: the PingPath adapter
//! (packets in and out), the current route to an address, and adding and
//! removing routes. Backends: Wintun and IP Helper on Windows, TUN and
//! `ip` on Linux (for tests against a real kernel), and [`fake`], an
//! in-memory network for engine tests.

use std::io;
use std::net::{IpAddr, Ipv4Addr};

use ipnet::IpNet;
use serde::Serialize;
use tokio::sync::mpsc;

pub mod fake;

/// The adapter's name, also used to find and delete a stale one.
pub const ADAPTER_NAME: &str = "PingPath";
/// Packets waiting in each direction between the adapter and the engine;
/// more are dropped rather than buffered (game traffic is real-time).
pub const PACKET_QUEUE: usize = 1024;

/// An interface: its LUID on Windows, its index on Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct InterfaceId(pub u64);

/// How the system currently reaches an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct NextHop {
    /// `None` when the address is on-link.
    pub gateway: Option<IpAddr>,
    pub interface: InterfaceId,
}

/// A route the engine adds and later removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Route {
    /// A game prefix through the PingPath adapter.
    Adapter {
        prefix: IpNet,
        interface: InterfaceId,
    },
    /// A relay's host route through the physical network.
    Host { address: IpAddr, via: NextHop },
}

impl Route {
    pub fn prefix(&self) -> IpNet {
        match *self {
            Route::Adapter { prefix, .. } => prefix,
            Route::Host { address, .. } => IpNet::from(address),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterConfig {
    pub address: Ipv4Addr,
    pub mtu: u16,
}

/// The adapter's packets: IP packets the system routed to it, and packets
/// to deliver to the system.
pub struct PacketChannels {
    pub incoming: mpsc::Receiver<Vec<u8>>,
    pub outgoing: mpsc::Sender<Vec<u8>>,
}

/// The PingPath adapter. Dropping it stops its I/O and removes it, and
/// the system removes every route through it with it.
pub trait Adapter: Send + Sync {
    fn interface(&self) -> InterfaceId;
}

/// What a cleanup found and removed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CleanupReport {
    pub adapters: usize,
    pub routes: usize,
}

pub trait Platform: Send + Sync + 'static {
    /// Creates the PingPath adapter with its address and MTU, removing a
    /// stale one left by a crash first.
    fn create_adapter(
        &self,
        config: &AdapterConfig,
    ) -> io::Result<(Box<dyn Adapter>, PacketChannels)>;

    /// The route the system uses for `dst` right now.
    fn best_route(&self, dst: IpAddr) -> io::Result<NextHop>;

    fn add_route(&self, route: &Route) -> io::Result<()>;

    /// Removes a route; removing one that is already gone is not an error.
    fn remove_route(&self, route: &Route) -> io::Result<()>;

    /// Removes everything PingPath left behind after a crash: a stale
    /// adapter and every route marked as ours, and nothing else.
    fn cleanup(&self) -> io::Result<CleanupReport>;
}
