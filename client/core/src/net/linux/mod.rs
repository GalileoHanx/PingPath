//! The Linux backend: a TUN device (tun.rs) and the kernel's routes
//! through iproute2's `ip` (ip.rs). Linux runs the client for development
//! and for the network-namespace tests (tests/netns.rs), which check the
//! engine's routing against a real kernel; players use Windows.
//!
//! As with Wintun, the adapter lives only as long as the process: the
//! kernel deletes a TUN device, and every route through it, when its file
//! descriptor closes. PingPath's routes carry protocol 80, so `cleanup`
//! and `best_route` can tell them from the system's.

pub mod ip;
pub mod tun;

use std::io;
use std::net::IpAddr;
use std::sync::Arc;

use ipnet::IpNet;

use self::tun::Tun;
use super::{
    ADAPTER_NAME, Adapter, AdapterConfig, CleanupReport, InterfaceId, NextHop, PACKET_QUEUE,
    PacketChannels, Platform, Route,
};

/// The Linux backend. The adapter is called "PingPath" unless a test picks
/// another name.
pub struct LinuxPlatform {
    adapter_name: String,
}

impl LinuxPlatform {
    pub fn new(adapter_name: &str) -> Self {
        Self {
            adapter_name: adapter_name.to_string(),
        }
    }
}

impl Default for LinuxPlatform {
    fn default() -> Self {
        Self::new(ADAPTER_NAME)
    }
}

struct LinuxAdapter {
    interface: InterfaceId,
    name: String,
    tasks: [tokio::task::JoinHandle<()>; 2],
}

impl Adapter for LinuxAdapter {
    fn interface(&self) -> InterfaceId {
        self.interface
    }
}

impl Drop for LinuxAdapter {
    fn drop(&mut self) {
        // The routes through the adapter must be gone when this returns.
        // Aborting the tasks closes the device only once the runtime drops
        // their futures, later; taking the link down removes its routes at
        // once.
        if let Err(error) = ip::ip(&["link", "set", "dev", &self.name, "down"]) {
            tracing::warn!(%error, "cannot take the adapter down");
        }
        for task in &self.tasks {
            task.abort();
        }
        tracing::info!(adapter = %self.name, "adapter removed");
    }
}

fn dev_name(interface: InterfaceId) -> io::Result<String> {
    let index = u32::try_from(interface.0)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "not a Linux ifindex"))?;
    ip::name_of(index)
}

impl Platform for LinuxPlatform {
    fn create_adapter(
        &self,
        config: &AdapterConfig,
    ) -> io::Result<(Box<dyn Adapter>, PacketChannels)> {
        // A TUN device goes with its process, so one already here belongs to
        // a process that is still running or was made persistent.
        if ip::delete_link(&self.adapter_name)? {
            tracing::warn!(adapter = %self.adapter_name, "removed a leftover adapter");
        }
        let tun = Arc::new(Tun::open(&self.adapter_name)?);
        let name = tun.name().to_owned();
        ip::configure_adapter(&name, config.address, config.mtu)?;
        let index = ip::index_of(&name)?;
        let pump = tun::pump(tun, PACKET_QUEUE);
        tracing::info!(adapter = %name, index, mtu = config.mtu, "adapter created");
        Ok((
            Box::new(LinuxAdapter {
                interface: InterfaceId(u64::from(index)),
                name,
                tasks: pump.tasks,
            }),
            PacketChannels {
                incoming: pump.incoming,
                outgoing: pump.outgoing,
            },
        ))
    }

    fn best_route(&self, dst: IpAddr) -> io::Result<NextHop> {
        let (gateway, dev) = ip::physical_route(dst, &self.adapter_name)?;
        Ok(NextHop {
            gateway,
            interface: InterfaceId(u64::from(ip::index_of(&dev)?)),
        })
    }

    fn add_route(&self, route: &Route) -> io::Result<()> {
        let result = match *route {
            Route::Adapter { prefix, interface } => {
                ip::route_add(prefix.trunc(), None, &dev_name(interface)?)
            }
            Route::Host { address, via } => {
                ip::route_add(IpNet::from(address), via.gateway, &dev_name(via.interface)?)
            }
        };
        match &result {
            Ok(()) => tracing::info!(route = %route.prefix(), "route added"),
            Err(error) => tracing::warn!(route = %route.prefix(), %error, "cannot add route"),
        }
        result
    }

    fn remove_route(&self, route: &Route) -> io::Result<()> {
        let interface = match *route {
            Route::Adapter { interface, .. } => interface,
            Route::Host { via, .. } => via.interface,
        };
        let dev = match dev_name(interface) {
            Ok(dev) => dev,
            // The interface is gone, and its routes with it.
            Err(_) => return Ok(()),
        };
        ip::route_del(route.prefix().trunc(), &dev)?;
        tracing::info!(route = %route.prefix(), "route removed");
        Ok(())
    }

    fn cleanup(&self) -> io::Result<CleanupReport> {
        let adapters = usize::from(ip::delete_link(&self.adapter_name)?);
        let routes = ip::flush_ours()?;
        if adapters + routes > 0 {
            tracing::warn!(adapters, routes, "removed what a previous run left behind");
        }
        Ok(CleanupReport { adapters, routes })
    }
}
