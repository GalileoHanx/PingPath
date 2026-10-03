//! The client end of a session: HELLO with a token, a WireGuard handshake
//! with the exit relay, then UDP datagrams in and out of the tunnel. Used by
//! the relay's tests today and by the Windows client in Phase 3.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::PublicKey;
use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::time::{Instant, timeout_at};

use crate::frame::{self, Cookie, Frame, HelloStatus, SessionId, TUNNEL_MTU};
use crate::keys::{self, WireGuardKeys};
use crate::packet::{self, TUNNEL_ECHO};

/// Room for any datagram plus WireGuard's overhead (and at least the 148
/// bytes a handshake initiation needs).
const BUFFER_LEN: usize = 65_535 + 64;
/// The address the client's side of the tunnel uses for inner packets.
pub const DEFAULT_INNER_ADDRESS: Ipv4Addr = Ipv4Addr::new(10, 200, 0, 2);
/// How long to wait before resending a HELLO or a handshake initiation.
const RETRY: Duration = Duration::from_millis(500);

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("network error: {0}")]
    Io(#[from] std::io::Error),
    #[error("the relay refused the session: {0:?}")]
    Refused(HelloStatus),
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    #[error("tunnel error: {0}")]
    Tunnel(String),
    #[error("key error: {0}")]
    Key(#[from] keys::KeyError),
    #[error("datagram of {0} bytes is larger than the tunnel allows ({max})", max = packet::MAX_TUNNEL_UDP_PAYLOAD)]
    TooLarge(usize),
}

/// A UDP datagram that came out of the tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Datagram {
    pub src: SocketAddrV4,
    pub dst: SocketAddrV4,
    pub payload: Vec<u8>,
}

pub struct ClientSession {
    socket: UdpSocket,
    session: SessionId,
    tunn: Tunn,
    inner: Ipv4Addr,
    buf: Vec<u8>,
    out: Vec<u8>,
    /// The entry's cookie for our current address, once it asked for one.
    cookie: Option<Cookie>,
    /// Set when the exit reports it has no tunnel for us (new session or a
    /// restart); the next send starts a handshake.
    needs_handshake: bool,
    echo_seq: u32,
}

impl ClientSession {
    /// Starts a session through `relay` (an entry relay, or the exit itself)
    /// with a token from the control plane, then completes the WireGuard
    /// handshake with the exit.
    pub async fn connect(
        relay: SocketAddr,
        token: &[u8],
        keys: &WireGuardKeys,
        exit_key: PublicKey,
        timeout: Duration,
    ) -> Result<Self, ClientError> {
        let claims = crate::token::decode_unverified(token)
            .map_err(|e| ClientError::Tunnel(e.to_string()))?;
        let socket = UdpSocket::bind(if relay.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })
        .await?;
        socket.connect(relay).await?;
        let index =
            u32::from_le_bytes(keys::random_32()?[..4].try_into().expect("4 bytes")) & 0x00ff_ffff;
        let tunn = Tunn::new(keys.secret.clone(), exit_key, None, None, index, None);
        let mut session = Self {
            socket,
            session: claims.session_id,
            tunn,
            inner: DEFAULT_INNER_ADDRESS,
            buf: vec![0; BUFFER_LEN],
            out: vec![0; BUFFER_LEN],
            cookie: None,
            needs_handshake: true,
            echo_seq: 0,
        };
        session.hello(token, timeout).await?;
        session.handshake(timeout).await?;
        Ok(session)
    }

    pub fn session_id(&self) -> SessionId {
        self.session
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Moves to a new local socket, as when the network changes (Wi-Fi to
    /// cable) or a NAT forgets the mapping. Through an entry relay, send
    /// [`hello`](Self::hello) next so it learns the new address; then a
    /// [`handshake`](Self::handshake), because the exit only moves its return
    /// path to an address that completes one.
    pub async fn rebind(&mut self) -> std::io::Result<()> {
        let peer = self.socket.peer_addr()?;
        let socket = UdpSocket::bind(if peer.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })
        .await?;
        socket.connect(peer).await?;
        self.socket = socket;
        self.cookie = None;
        Ok(())
    }

    /// Sends HELLO (also used to renew with a fresh token) and waits for the
    /// relay's answer, resending in case either datagram is lost and
    /// answering an entry relay's `Verify` with its cookie.
    pub async fn hello(&mut self, token: &[u8], timeout: Duration) -> Result<(), ClientError> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.socket
                .send(&frame::hello(&self.session, token, self.cookie.as_ref()))
                .await?;
            let attempt = (Instant::now() + RETRY).min(deadline);
            loop {
                let Ok(received) = timeout_at(attempt, self.socket.recv(&mut self.buf)).await
                else {
                    break;
                };
                let n = received?;
                let Ok(Frame::HelloAck {
                    session,
                    status,
                    cookie,
                }) = Frame::parse(&self.buf[..n])
                else {
                    continue;
                };
                if session != self.session {
                    continue;
                }
                match status {
                    HelloStatus::Ok => return Ok(()),
                    HelloStatus::OkNewSession => {
                        self.needs_handshake = true;
                        return Ok(());
                    }
                    HelloStatus::Verify => {
                        self.cookie = cookie;
                        break; // resend now, with the cookie
                    }
                    refused => return Err(ClientError::Refused(refused)),
                }
            }
        }
        Err(ClientError::Timeout("HELLO_ACK"))
    }

    /// Completes a WireGuard handshake with the exit, resending the
    /// initiation every half second until it answers.
    pub async fn handshake(&mut self, timeout: Duration) -> Result<(), ClientError> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.tick().await?;
            if let TunnResult::WriteToNetwork(init) =
                self.tunn.format_handshake_initiation(&mut self.out, true)
            {
                let frame = frame::data(&self.session, init);
                self.socket.send(&frame).await?;
            }
            let attempt = (Instant::now() + RETRY).min(deadline);
            while let Ok(received) = timeout_at(attempt, self.socket.recv(&mut self.buf)).await {
                let n = received?;
                if self.handle(n).await?.handshake_completed {
                    self.needs_handshake = false;
                    return Ok(());
                }
            }
        }
        Err(ClientError::Timeout("the WireGuard handshake"))
    }

    /// Sends one UDP datagram into the tunnel, from `src_port` on the
    /// client's inner address to `dst`.
    pub async fn send_udp(
        &mut self,
        src_port: u16,
        dst: SocketAddrV4,
        payload: &[u8],
    ) -> Result<(), ClientError> {
        if payload.len() > packet::MAX_TUNNEL_UDP_PAYLOAD {
            return Err(ClientError::TooLarge(payload.len()));
        }
        if self.needs_handshake {
            self.handshake(Duration::from_secs(5)).await?;
        }
        let inner = packet::build_udp(SocketAddrV4::new(self.inner, src_port), dst, payload);
        debug_assert!(inner.len() <= TUNNEL_MTU);
        let frame = match self.tunn.encapsulate(&inner, &mut self.out) {
            TunnResult::WriteToNetwork(message) => Some(frame::data(&self.session, message)),
            TunnResult::Err(e) => return Err(ClientError::Tunnel(format!("{e:?}"))),
            _ => None, // queued until a handshake completes
        };
        if let Some(frame) = frame {
            self.socket.send(&frame).await?;
        }
        Ok(())
    }

    /// Waits up to `timeout` for the next UDP datagram out of the tunnel,
    /// answering handshakes and keepalives on the way.
    pub async fn recv_udp(&mut self, timeout: Duration) -> Result<Datagram, ClientError> {
        let deadline = Instant::now() + timeout;
        loop {
            let received = timeout_at(deadline, self.socket.recv(&mut self.buf))
                .await
                .map_err(|_| ClientError::Timeout("a datagram from the tunnel"))?;
            let n = received?;
            if let Some(datagram) = self.handle(n).await?.datagram {
                return Ok(datagram);
            }
        }
    }

    /// An authenticated round trip to the exit through the tunnel (the
    /// exit's in-tunnel echo). Phase 3's watchdog sends one every 250 ms.
    pub async fn echo(&mut self, timeout: Duration) -> Result<Duration, ClientError> {
        self.echo_seq = self.echo_seq.wrapping_add(1);
        let payload = self.echo_seq.to_be_bytes();
        let start = Instant::now();
        self.send_udp(TUNNEL_ECHO.port(), TUNNEL_ECHO, &payload)
            .await?;
        let deadline = start + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let datagram = self.recv_udp(remaining).await?;
            if datagram.src == TUNNEL_ECHO && datagram.payload == payload {
                return Ok(start.elapsed());
            }
        }
    }

    /// Runs WireGuard's timers (rekeying, keepalives, and the clock and
    /// rate-limiter reset `boringtun` keeps there); call a few times a second.
    /// The other methods call it as they go.
    pub async fn tick(&mut self) -> Result<(), ClientError> {
        let frame = match self.tunn.update_timers(&mut self.out) {
            TunnResult::WriteToNetwork(message) => Some(frame::data(&self.session, message)),
            _ => None,
        };
        if let Some(frame) = frame {
            self.socket.send(&frame).await?;
        }
        Ok(())
    }

    /// Processes one received frame.
    async fn handle(&mut self, n: usize) -> Result<Handled, ClientError> {
        let mut handled = Handled::default();
        let message = match Frame::parse(&self.buf[..n]) {
            Ok(Frame::Data { session, message }) if session == self.session => message.to_vec(),
            Ok(Frame::HelloAck {
                session,
                status: HelloStatus::OkNewSession,
                ..
            }) if session == self.session => {
                self.needs_handshake = true;
                return Ok(handled);
            }
            _ => return Ok(handled),
        };
        self.tick().await?;
        let relay = self.socket.peer_addr().ok().map(|a| a.ip());
        let mut replies = Vec::new();
        let mut next: &[u8] = &message;
        loop {
            match self
                .tunn
                .decapsulate(relay.filter(|_| !next.is_empty()), next, &mut self.out)
            {
                TunnResult::WriteToNetwork(reply) => {
                    // Accepting a handshake response yields the confirming keepalive.
                    if next.first() == Some(&2) {
                        handled.handshake_completed = true;
                    }
                    replies.push(frame::data(&self.session, reply));
                    next = &[]; // drain queued packets
                }
                TunnResult::WriteToTunnelV4(inner, _) => {
                    if let Ok(udp) = packet::parse_udp(inner) {
                        handled.datagram = Some(Datagram {
                            src: udp.src,
                            dst: udp.dst,
                            payload: udp.payload.to_vec(),
                        });
                    }
                    break;
                }
                TunnResult::Done | TunnResult::WriteToTunnelV6(..) => break,
                TunnResult::Err(e) => {
                    tracing::debug!(error = ?e, "tunnel rejected a message");
                    break;
                }
            }
        }
        for reply in replies {
            self.socket.send(&reply).await?;
        }
        Ok(handled)
    }
}

#[derive(Default)]
struct Handled {
    datagram: Option<Datagram>,
    handshake_completed: bool,
}
