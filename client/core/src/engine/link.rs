//! The session's network side: one unconnected UDP socket (per address
//! family) for every relay it talks to, the current one and the standbys,
//! and the entry cookies issued to that socket.
//!
//! One unconnected socket means the source address follows the routing
//! table (a switch from Wi-Fi to cable needs no new socket), and a cookie
//! fetched ahead of time from a standby entry is still valid when traffic
//! moves there, which saves a round trip on failover
//! (docs/decisions/009-path-selection-and-failover.md).

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use pingpath_tunnel::core::{Event, TunnelCore};
use pingpath_tunnel::frame::{Cookie, HelloStatus};
use serde::Serialize;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// The relay a path sends to: an entry, or the exit itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hop {
    pub id: u32,
    pub name: String,
    pub address: SocketAddr,
    /// Through an entry (which asks for a cookie) or straight to the exit.
    pub via_entry: bool,
}

pub struct Link {
    v4: Option<UdpSocket>,
    v6: Option<UdpSocket>,
    cookies: HashMap<SocketAddr, Cookie>,
    buf4: Vec<u8>,
    buf6: Vec<u8>,
}

impl Link {
    /// Sockets for the address families `hops` use.
    pub async fn bind(hops: &[Hop]) -> io::Result<Self> {
        let v4 = if hops.iter().any(|h| h.address.is_ipv4()) {
            Some(UdpSocket::bind("0.0.0.0:0").await?)
        } else {
            None
        };
        let v6 = if hops.iter().any(|h| h.address.is_ipv6()) {
            Some(UdpSocket::bind("[::]:0").await?)
        } else {
            None
        };
        Ok(Self {
            v4,
            v6,
            cookies: HashMap::new(),
            buf4: vec![0; 65_535],
            buf6: vec![0; 65_535],
        })
    }

    pub async fn send(&self, to: SocketAddr, frame: &[u8]) -> io::Result<()> {
        let socket = if to.is_ipv4() { &self.v4 } else { &self.v6 };
        match socket {
            Some(socket) => socket.send_to(frame, to).await.map(|_| ()),
            None => Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "no socket for that address family",
            )),
        }
    }

    /// The next datagram from any relay. Cancel-safe.
    pub async fn recv(&mut self) -> io::Result<(Vec<u8>, SocketAddr)> {
        let (v4, v6) = (self.v4.as_ref(), self.v6.as_ref());
        let (buf4, buf6) = (&mut self.buf4, &mut self.buf6);
        tokio::select! {
            r = recv_on(v4, buf4) => r.map(|(n, from)| (buf4[..n].to_vec(), from)),
            r = recv_on(v6, buf6) => r.map(|(n, from)| (buf6[..n].to_vec(), from)),
        }
    }

    pub fn cookie(&self, relay: SocketAddr) -> Option<&Cookie> {
        self.cookies.get(&relay)
    }

    pub fn set_cookie(&mut self, relay: SocketAddr, cookie: Cookie) {
        self.cookies.insert(relay, cookie);
    }

    /// Forgets every cookie: they are tied to our address, which changed.
    pub fn forget_cookies(&mut self) {
        self.cookies.clear();
    }

    /// HELLO for `hop`, with its cookie if we have one.
    pub fn hello(&self, core: &TunnelCore, token: &[u8], hop: &Hop) -> Vec<u8> {
        core.hello_with(token, self.cookie(hop.address))
    }
}

async fn recv_on(socket: Option<&UdpSocket>, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    match socket {
        Some(socket) => socket.recv_from(buf).await,
        None => std::future::pending().await,
    }
}

/// Resend the current step after this long without an answer (the first
/// connection; failover uses the measured round trip).
const RETRY: Duration = Duration::from_millis(200);

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("the relay refused the session: {0:?}")]
    Refused(HelloStatus),
    #[error("no answer from {relay} while waiting for {waiting_for}")]
    Timeout {
        relay: String,
        waiting_for: &'static str,
    },
    #[error("network error: {0}")]
    Io(#[from] io::Error),
}

/// What arrived while connecting, besides the handshake.
#[derive(Debug, Default)]
pub struct Connected {
    /// IP packets out of the tunnel (from the exit, already authenticated).
    pub packets: Vec<Vec<u8>>,
    /// The exit had no session for us (it restarted, or this is a new
    /// session): a new set of flows, so new ports at the game server.
    pub new_session: bool,
}

/// Brings the session up through `hop` before `deadline`: HELLO (with the
/// entry's cookie when we have one; else the entry's Verify comes first),
/// then the handshake, pipelined: the HELLO that carries the cookie and the
/// handshake initiation go out together, because the entry records the
/// session before the handshake reaches it. The path is up when the
/// handshake completes, which proves it works both ways; the confirming
/// keepalive that goes out with it makes the exit move its return path
/// here (ADR 004). Packets that reach the adapter meanwhile are dropped
/// (they would arrive stale) when `drain` is given.
pub async fn connect(
    core: &mut TunnelCore,
    link: &mut Link,
    hop: &Hop,
    token: &[u8],
    deadline: Instant,
    mut drain: Option<&mut mpsc::Receiver<Vec<u8>>>,
) -> Result<Connected, ConnectError> {
    let mut connected = Connected::default();
    // Waiting for a cookie first only through an entry we have none for.
    let mut verified = !hop.via_entry || link.cookie(hop.address).is_some();
    let send_step = |core: &mut TunnelCore, link: &Link, verified: bool| {
        let mut frames = vec![link.hello(core, token, hop)];
        if verified {
            frames.extend(core.handshake_initiation());
        }
        frames
    };
    for frame in send_step(core, link, verified) {
        link.send(hop.address, &frame).await?;
    }
    let mut last_sent = Instant::now();
    loop {
        let retry_at = (last_sent + RETRY).min(deadline);
        tokio::select! {
            received = link.recv() => {
                let (datagram, from) = match received {
                    Ok(received) => received,
                    // An ICMP error for an earlier send (a closed port).
                    Err(e) => {
                        tracing::debug!(relay = %hop.name, error = %e, "receive error while connecting");
                        continue;
                    }
                };
                if from != hop.address {
                    continue;
                }
                let handled = core.handle(&datagram, Some(from.ip()));
                for frame in &handled.send {
                    link.send(from, frame).await?;
                }
                for event in handled.events {
                    match event {
                        Event::Verify(cookie) => {
                            link.set_cookie(from, cookie);
                            verified = true;
                            for frame in send_step(core, link, verified) {
                                link.send(from, &frame).await?;
                            }
                            last_sent = Instant::now();
                        }
                        Event::Accepted { needs_handshake } => {
                            connected.new_session |= needs_handshake;
                            if !verified {
                                verified = true;
                                if let Some(init) = core.handshake_initiation() {
                                    link.send(from, &init).await?;
                                }
                                last_sent = Instant::now();
                            }
                        }
                        Event::CookieReply => {
                            if let Some(init) = core.handshake_initiation() {
                                link.send(from, &init).await?;
                            }
                        }
                        Event::Refused(status) => return Err(ConnectError::Refused(status)),
                        Event::HandshakeCompleted => return Ok(connected),
                        Event::Packet(packet) => connected.packets.push(packet),
                        Event::Echo(_) => {}
                    }
                }
            }
            Some(_) = async {
                match drain.as_deref_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {}
            _ = tokio::time::sleep_until(retry_at) => {
                if Instant::now() >= deadline {
                    return Err(ConnectError::Timeout {
                        relay: hop.name.clone(),
                        waiting_for: if verified { "the WireGuard handshake" } else { "the entry's cookie" },
                    });
                }
                for frame in send_step(core, link, verified) {
                    link.send(hop.address, &frame).await?;
                }
                last_sent = Instant::now();
            }
        }
    }
}
