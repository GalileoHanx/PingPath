//! The client end of a session over one socket: HELLO with a token, a
//! WireGuard handshake with the exit relay, then UDP datagrams in and out of
//! the tunnel. A thin async wrapper around [`TunnelCore`], used by tests and
//! tools; the Windows client drives [`TunnelCore`] from its own event loop.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use boringtun::x25519::PublicKey;
use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::time::{Instant, timeout_at};

use crate::core::{Event, SendError, TunnelCore};
use crate::frame::{HelloStatus, SessionId};
use crate::keys::{self, WireGuardKeys};
use crate::packet;

/// Room for any datagram.
const BUFFER_LEN: usize = 65_535;
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
    core: TunnelCore,
    inner: Ipv4Addr,
    buf: Vec<u8>,
}

impl From<SendError> for ClientError {
    fn from(e: SendError) -> Self {
        match e {
            SendError::TooLarge(n) => ClientError::TooLarge(n),
            SendError::Tunnel(e) => ClientError::Tunnel(e),
        }
    }
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
        let core = TunnelCore::new(
            claims.session_id,
            keys.secret.clone(),
            exit_key,
            DEFAULT_INNER_ADDRESS,
        )?;
        let mut session = Self {
            socket,
            core,
            inner: DEFAULT_INNER_ADDRESS,
            buf: vec![0; BUFFER_LEN],
        };
        session.hello(token, timeout).await?;
        session.handshake(timeout).await?;
        Ok(session)
    }

    pub fn session_id(&self) -> SessionId {
        self.core.session_id()
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
        self.core.forget_cookie();
        Ok(())
    }

    /// Sends HELLO (also used to renew with a fresh token) and waits for the
    /// relay's answer, resending in case either datagram is lost and
    /// answering an entry relay's `Verify` with its cookie.
    pub async fn hello(&mut self, token: &[u8], timeout: Duration) -> Result<(), ClientError> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.socket.send(&self.core.hello(token)).await?;
            let attempt = (Instant::now() + RETRY).min(deadline);
            'attempt: while let Ok(received) =
                timeout_at(attempt, self.socket.recv(&mut self.buf)).await
            {
                let n = received?;
                for event in self.process(n).await?.events {
                    match event {
                        Event::Accepted { .. } => return Ok(()),
                        Event::Verify(_) => break 'attempt, // resend now, with the cookie
                        Event::Refused(status) => return Err(ClientError::Refused(status)),
                        _ => {}
                    }
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
            if let Some(init) = self.core.handshake_initiation() {
                self.socket.send(&init).await?;
            }
            let attempt = (Instant::now() + RETRY).min(deadline);
            while let Ok(received) = timeout_at(attempt, self.socket.recv(&mut self.buf)).await {
                let n = received?;
                if self
                    .process(n)
                    .await?
                    .events
                    .contains(&Event::HandshakeCompleted)
                {
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
        let inner = packet::build_udp(SocketAddrV4::new(self.inner, src_port), dst, payload);
        self.send_packet(&inner).await
    }

    /// Sends one IP packet into the tunnel, completing a handshake first if
    /// the exit has none for this session.
    pub async fn send_packet(&mut self, packet: &[u8]) -> Result<(), ClientError> {
        if self.core.needs_handshake() {
            self.handshake(Duration::from_secs(5)).await?;
        }
        if let Some(frame) = self.core.encapsulate(packet)? {
            self.socket.send(&frame).await?;
        }
        Ok(())
    }

    /// Waits up to `timeout` for the next UDP datagram out of the tunnel,
    /// answering handshakes and keepalives on the way.
    pub async fn recv_udp(&mut self, timeout: Duration) -> Result<Datagram, ClientError> {
        let deadline = Instant::now() + timeout;
        loop {
            let packet = self.recv_until(deadline).await?;
            if let Ok(udp) = packet::parse_udp(&packet) {
                return Ok(Datagram {
                    src: udp.src,
                    dst: udp.dst,
                    payload: udp.payload.to_vec(),
                });
            }
        }
    }

    /// Waits up to `timeout` for the next IP packet out of the tunnel.
    pub async fn recv_packet(&mut self, timeout: Duration) -> Result<Vec<u8>, ClientError> {
        self.recv_until(Instant::now() + timeout).await
    }

    async fn recv_until(&mut self, deadline: Instant) -> Result<Vec<u8>, ClientError> {
        loop {
            let received = timeout_at(deadline, self.socket.recv(&mut self.buf))
                .await
                .map_err(|_| ClientError::Timeout("a datagram from the tunnel"))?;
            let n = received?;
            for event in self.process(n).await?.events {
                if let Event::Packet(packet) = event {
                    return Ok(packet);
                }
            }
        }
    }

    /// An authenticated round trip to the exit through the tunnel (the
    /// exit's in-tunnel echo). The Windows client's watchdog sends one every
    /// 250 ms.
    pub async fn echo(&mut self, timeout: Duration) -> Result<Duration, ClientError> {
        if self.core.needs_handshake() {
            self.handshake(Duration::from_secs(5)).await?;
        }
        let start = Instant::now();
        let deadline = start + timeout;
        let (seq, frame) = self.core.echo()?;
        if let Some(frame) = frame {
            self.socket.send(&frame).await?;
        }
        loop {
            let received = timeout_at(deadline, self.socket.recv(&mut self.buf))
                .await
                .map_err(|_| ClientError::Timeout("the echo reply"))?;
            let n = received?;
            if self.process(n).await?.events.contains(&Event::Echo(seq)) {
                return Ok(start.elapsed());
            }
        }
    }

    /// Runs WireGuard's timers (rekeying, keepalives, and the clock and
    /// rate-limiter reset `boringtun` keeps there); call a few times a second.
    /// The other methods call it as they go.
    pub async fn tick(&mut self) -> Result<(), ClientError> {
        for frame in self.core.tick() {
            self.socket.send(&frame).await?;
        }
        Ok(())
    }

    /// Processes one received datagram and sends what it produced.
    async fn process(&mut self, n: usize) -> Result<crate::core::Handled, ClientError> {
        let relay = self.socket.peer_addr().ok().map(|a| a.ip());
        let handled = self.core.handle(&self.buf[..n], relay);
        for frame in &handled.send {
            self.socket.send(frame).await?;
        }
        Ok(handled)
    }
}
