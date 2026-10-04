//! An in-memory network for engine tests on any OS, without privileges.
//!
//! A [`FakeGame`] sends UDP the way a game on the player's PC would: the
//! fake looks up the routes the engine installed, and a packet to a prefix
//! routed to the adapter becomes an IPv4 packet the engine reads from the
//! adapter, while anything else goes out directly through a real UDP
//! socket. Packets the engine writes to the adapter are delivered back to
//! the game. Like Windows, removing the adapter removes its routes, so
//! dropping the engine (a crash) sends traffic direct again.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pingpath_tunnel::packet;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use super::{
    Adapter, AdapterConfig, CleanupReport, InterfaceId, NextHop, PACKET_QUEUE, PacketChannels,
    Platform, Route,
};

/// The fake's physical interface; the adapter gets another ID each time.
pub const PHYSICAL: InterfaceId = InterfaceId(1);

/// Which way a packet went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    Tunnel,
    Direct,
}

/// A route change, with when it happened, for timing assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteEvent {
    pub at: Instant,
    pub added: bool,
    pub route: Route,
}

#[derive(Default)]
struct State {
    routes: Vec<Route>,
    history: Vec<RouteEvent>,
    adapter: Option<AdapterEnds>,
    next_interface: u64,
    /// Game inner port -> where its tunnel replies go.
    games: HashMap<u16, mpsc::Sender<(SocketAddrV4, Vec<u8>, Path)>>,
    /// Destinations of packets that went through the adapter.
    tunneled: Vec<SocketAddrV4>,
    fail_adapter: bool,
}

struct AdapterEnds {
    interface: InterfaceId,
    address: std::net::Ipv4Addr,
    to_engine: mpsc::Sender<Vec<u8>>,
}

/// The fake network. Cheap to clone; clones share state.
#[derive(Clone, Default)]
pub struct FakeNetwork {
    state: Arc<Mutex<State>>,
}

impl FakeNetwork {
    pub fn new() -> Self {
        Self::default()
    }

    /// The routes installed now.
    pub fn routes(&self) -> Vec<Route> {
        self.lock().routes.clone()
    }

    /// Every route change so far.
    pub fn history(&self) -> Vec<RouteEvent> {
        self.lock().history.clone()
    }

    /// Destinations of every packet that went through the adapter.
    pub fn tunneled(&self) -> Vec<SocketAddrV4> {
        self.lock().tunneled.clone()
    }

    pub fn has_adapter(&self) -> bool {
        self.lock().adapter.is_some()
    }

    /// Makes the next `create_adapter` fail, as when wintun.dll is missing.
    pub fn fail_next_adapter(&self) {
        self.lock().fail_adapter = true;
    }

    /// Which way a packet to `dst` would go now: longest prefix match
    /// among the adapter's routes, else direct.
    pub fn path_to(&self, dst: IpAddr) -> Path {
        let state = self.lock();
        let Some(adapter) = &state.adapter else {
            return Path::Direct;
        };
        let best = state
            .routes
            .iter()
            .filter(|r| r.prefix().contains(&dst))
            .max_by_key(|r| r.prefix().prefix_len());
        match best {
            Some(Route::Adapter { interface, .. }) if *interface == adapter.interface => {
                Path::Tunnel
            }
            _ => Path::Direct,
        }
    }

    /// A game on the player's PC, with its own UDP port.
    pub async fn game(&self) -> io::Result<FakeGame> {
        let direct = UdpSocket::bind("127.0.0.1:0").await?;
        let port = direct.local_addr()?.port();
        let (tx, rx) = mpsc::channel(PACKET_QUEUE);
        self.lock().games.insert(port, tx);
        Ok(FakeGame {
            net: self.clone(),
            direct: Arc::new(direct),
            port,
            replies: tokio::sync::Mutex::new(rx),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("fake network lock")
    }
}

struct FakeAdapter {
    net: FakeNetwork,
    interface: InterfaceId,
}

impl Adapter for FakeAdapter {
    fn interface(&self) -> InterfaceId {
        self.interface
    }
}

impl Drop for FakeAdapter {
    fn drop(&mut self) {
        let mut state = self.net.lock();
        if state.adapter.as_ref().map(|a| a.interface) == Some(self.interface) {
            state.adapter = None;
        }
        // Routes through a removed interface disappear with it.
        let interface = self.interface;
        let now = Instant::now();
        let (gone, kept): (Vec<Route>, Vec<Route>) = state
            .routes
            .drain(..)
            .partition(|r| matches!(r, Route::Adapter { interface: i, .. } if *i == interface));
        state.routes = kept;
        for route in gone {
            state.history.push(RouteEvent {
                at: now,
                added: false,
                route,
            });
        }
    }
}

impl Platform for FakeNetwork {
    fn create_adapter(
        &self,
        config: &AdapterConfig,
    ) -> io::Result<(Box<dyn Adapter>, PacketChannels)> {
        let (to_engine, incoming) = mpsc::channel(PACKET_QUEUE);
        let (outgoing, mut from_engine) = mpsc::channel::<Vec<u8>>(PACKET_QUEUE);
        let interface = {
            let mut state = self.lock();
            if std::mem::take(&mut state.fail_adapter) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "wintun.dll not found (fake)",
                ));
            }
            state.next_interface += 1;
            let interface = InterfaceId(100 + state.next_interface);
            state.adapter = Some(AdapterEnds {
                interface,
                address: config.address,
                to_engine,
            });
            interface
        };
        // Packets the engine writes to the adapter go to the game whose
        // port they are addressed to.
        let net = self.clone();
        tokio::spawn(async move {
            while let Some(packet) = from_engine.recv().await {
                let Ok(udp) = packet::parse_udp(&packet) else {
                    continue;
                };
                let game = net.lock().games.get(&udp.dst.port()).cloned();
                if let Some(game) = game {
                    let _ = game.try_send((udp.src, udp.payload.to_vec(), Path::Tunnel));
                }
            }
        });
        Ok((
            Box::new(FakeAdapter {
                net: self.clone(),
                interface,
            }),
            PacketChannels { incoming, outgoing },
        ))
    }

    fn best_route(&self, _dst: IpAddr) -> io::Result<NextHop> {
        Ok(NextHop {
            gateway: Some("192.168.1.1".parse().expect("valid")),
            interface: PHYSICAL,
        })
    }

    fn add_route(&self, route: &Route) -> io::Result<()> {
        let mut state = self.lock();
        if let Route::Adapter { interface, .. } = route {
            if state.adapter.as_ref().map(|a| a.interface) != Some(*interface) {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no such interface"));
            }
        }
        if !state.routes.contains(route) {
            state.routes.push(*route);
            state.history.push(RouteEvent {
                at: Instant::now(),
                added: true,
                route: *route,
            });
        }
        Ok(())
    }

    fn remove_route(&self, route: &Route) -> io::Result<()> {
        let mut state = self.lock();
        if let Some(i) = state.routes.iter().position(|r| r == route) {
            state.routes.remove(i);
            state.history.push(RouteEvent {
                at: Instant::now(),
                added: false,
                route: *route,
            });
        }
        Ok(())
    }

    fn cleanup(&self) -> io::Result<CleanupReport> {
        let mut state = self.lock();
        let routes = state.routes.len();
        state.routes.clear();
        Ok(CleanupReport {
            adapters: 0,
            routes,
        })
    }
}

/// A UDP "game": sends through the fake network's routes and receives
/// from both paths.
pub struct FakeGame {
    net: FakeNetwork,
    direct: Arc<UdpSocket>,
    port: u16,
    replies: tokio::sync::Mutex<mpsc::Receiver<(SocketAddrV4, Vec<u8>, Path)>>,
}

impl FakeGame {
    /// Sends one datagram; returns the way it went.
    pub async fn send_to(&self, dst: SocketAddrV4, payload: &[u8]) -> io::Result<Path> {
        let path = self.net.path_to(IpAddr::V4(*dst.ip()));
        if path == Path::Tunnel {
            let to_engine = {
                let mut state = self.net.lock();
                state.tunneled.push(dst);
                state
                    .adapter
                    .as_ref()
                    .map(|a| (a.to_engine.clone(), a.address))
            };
            if let Some((to_engine, address)) = to_engine {
                let packet = packet::build_udp(SocketAddrV4::new(address, self.port), dst, payload);
                // A full queue drops the packet, as a real adapter would.
                let _ = to_engine.try_send(packet);
                return Ok(Path::Tunnel);
            }
        }
        self.direct.send_to(payload, SocketAddr::V4(dst)).await?;
        Ok(Path::Direct)
    }

    /// The next datagram from either path, within `timeout`.
    pub async fn recv(&self, timeout: Duration) -> io::Result<(SocketAddrV4, Vec<u8>, Path)> {
        let mut replies = self.replies.lock().await;
        let mut buf = vec![0u8; 2048];
        let receive = async {
            tokio::select! {
                Some(reply) = replies.recv() => Ok(reply),
                received = self.direct.recv_from(&mut buf) => {
                    let (n, from) = received?;
                    let SocketAddr::V4(from) = from else {
                        return Err(io::Error::other("IPv6 reply"));
                    };
                    Ok((from, buf[..n].to_vec(), Path::Direct))
                }
            }
        };
        tokio::time::timeout(timeout, receive)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no reply"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn routes_decide_the_path_and_vanish_with_the_adapter() {
        let net = FakeNetwork::new();
        let server: SocketAddrV4 = "127.0.0.1:9".parse().unwrap();
        assert_eq!(net.path_to(IpAddr::V4(*server.ip())), Path::Direct);

        let (adapter, mut channels) = net
            .create_adapter(&AdapterConfig {
                address: "10.200.0.2".parse().unwrap(),
                mtu: 1380,
            })
            .unwrap();
        let route = Route::Adapter {
            prefix: "127.0.0.0/8".parse().unwrap(),
            interface: adapter.interface(),
        };
        net.add_route(&route).unwrap();
        let game = net.game().await.unwrap();
        assert_eq!(game.send_to(server, b"hi").await.unwrap(), Path::Tunnel);
        let packet = channels.incoming.recv().await.unwrap();
        let udp = packet::parse_udp(&packet).unwrap();
        assert_eq!((udp.dst, udp.payload), (server, &b"hi"[..]));

        // A reply written to the adapter reaches the game.
        let reply = packet::build_udp(server, udp.src, b"back");
        channels.outgoing.send(reply).await.unwrap();
        let (from, payload, path) = game.recv(Duration::from_secs(1)).await.unwrap();
        assert_eq!(
            (from, payload.as_slice(), path),
            (server, &b"back"[..], Path::Tunnel)
        );

        drop(adapter);
        assert!(net.routes().is_empty());
        assert_eq!(net.path_to(IpAddr::V4(*server.ip())), Path::Direct);
    }
}
