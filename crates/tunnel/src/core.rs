//! The client end of a session without any I/O: HELLO and the entry's
//! cookie, the WireGuard handshake with the exit, IP packets in and out,
//! and the exit's in-tunnel echo. Callers own the sockets and the clock, so
//! one event loop can drive the adapter, several relay paths and the
//! watchdog together (the Windows client), while [`ClientSession`]
//! wraps it with a socket for simple uses and tests.
//!
//! [`ClientSession`]: crate::client::ClientSession

use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};

use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};

use crate::frame::{self, Cookie, Frame, HelloStatus, SessionId, TUNNEL_MTU};
use crate::keys;
use crate::packet::{self, TUNNEL_ECHO};

/// Room for any datagram plus WireGuard's overhead (and at least the 148
/// bytes a handshake initiation needs).
const BUFFER_LEN: usize = 65_535 + 64;
/// The source port of echo packets inside the tunnel.
pub const ECHO_SOURCE_PORT: u16 = 7;
/// WireGuard message types (the first byte of a message).
const WG_HANDSHAKE_RESPONSE: u8 = 2;
const WG_COOKIE_REPLY: u8 = 3;
const WG_DATA: u8 = 4;

/// Something the relay told us, from one received frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The relay accepted the HELLO. `needs_handshake` is set when the exit
    /// has no tunnel for us yet (a new session, or it restarted).
    Accepted { needs_handshake: bool },
    /// An entry relay wants proof of our address. The cookie is stored, so
    /// the next [`TunnelCore::hello`] carries it, and returned, so a caller
    /// that keeps one socket per relay can keep one cookie per relay.
    Verify(Cookie),
    /// The relay refused the session.
    Refused(HelloStatus),
    /// Our handshake with the exit completed: data flows from now on.
    HandshakeCompleted,
    /// An IP packet out of the tunnel, for the adapter.
    Packet(Vec<u8>),
    /// The exit answered the echo with this sequence number.
    Echo(u32),
    /// The exit is under load and sent a WireGuard cookie: send a fresh
    /// handshake initiation at once (it now carries the cookie).
    CookieReply,
}

/// What one received frame produced: events for the caller, and frames to
/// send back to the same relay (handshake confirmations, queued packets).
#[derive(Debug, Default)]
pub struct Handled {
    pub events: Vec<Event>,
    pub send: Vec<Vec<u8>>,
    /// The frame authenticated as coming from the exit (data, a keepalive,
    /// or a handshake response): proof the path works in that direction,
    /// whatever it carried.
    pub authenticated: bool,
}

/// Why a packet couldn't be sent into the tunnel.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    #[error("packet of {0} bytes is larger than the tunnel MTU ({TUNNEL_MTU})")]
    TooLarge(usize),
    #[error("tunnel error: {0}")]
    Tunnel(String),
}

pub struct TunnelCore {
    session: SessionId,
    tunn: Tunn,
    /// The entry's cookie for our current address, once it asked for one.
    cookie: Option<Cookie>,
    needs_handshake: bool,
    inner: Ipv4Addr,
    echo_seq: u32,
    buf: Vec<u8>,
}

impl TunnelCore {
    /// A session with the exit whose WireGuard key is `exit_key`. `inner` is
    /// the source address of the echo packets the core builds itself.
    pub fn new(
        session: SessionId,
        client_secret: StaticSecret,
        exit_key: PublicKey,
        inner: Ipv4Addr,
    ) -> Result<Self, keys::KeyError> {
        let index =
            u32::from_le_bytes(keys::random_32()?[..4].try_into().expect("4 bytes")) & 0x00ff_ffff;
        Ok(Self {
            session,
            tunn: Tunn::new(client_secret, exit_key, None, None, index, None),
            cookie: None,
            needs_handshake: true,
            inner,
            echo_seq: 0,
            buf: vec![0; BUFFER_LEN],
        })
    }

    pub fn session_id(&self) -> SessionId {
        self.session
    }

    /// Whether the exit has told us it has no tunnel for this session, or
    /// no handshake has completed yet.
    pub fn needs_handshake(&self) -> bool {
        self.needs_handshake
    }

    /// A HELLO with `token`, carrying the entry's cookie if it gave us one.
    pub fn hello(&self, token: &[u8]) -> Vec<u8> {
        frame::hello(&self.session, token, self.cookie.as_ref())
    }

    /// A HELLO with `token` and the given cookie (or none), for callers that
    /// keep a cookie per relay.
    pub fn hello_with(&self, token: &[u8], cookie: Option<&Cookie>) -> Vec<u8> {
        frame::hello(&self.session, token, cookie)
    }

    /// Forget the entry's cookie: it is only valid for the address and port
    /// it was issued to, so call this when switching socket or relay.
    pub fn forget_cookie(&mut self) {
        self.cookie = None;
    }

    /// A fresh handshake initiation, to start the tunnel or to move it to a
    /// new path (the exit only moves its return path after a handshake).
    pub fn handshake_initiation(&mut self) -> Option<Vec<u8>> {
        match self.tunn.format_handshake_initiation(&mut self.buf, true) {
            TunnResult::WriteToNetwork(init) => Some(frame::data(&self.session, init)),
            _ => None,
        }
    }

    /// Encrypts one IP packet. `Ok(None)` means it was queued until the
    /// handshake completes.
    pub fn encapsulate(&mut self, packet: &[u8]) -> Result<Option<Vec<u8>>, SendError> {
        if packet.len() > TUNNEL_MTU {
            return Err(SendError::TooLarge(packet.len()));
        }
        match self.tunn.encapsulate(packet, &mut self.buf) {
            TunnResult::WriteToNetwork(message) => Ok(Some(frame::data(&self.session, message))),
            TunnResult::Err(e) => Err(SendError::Tunnel(format!("{e:?}"))),
            _ => Ok(None),
        }
    }

    /// An echo request to the exit. Returns its sequence number and the
    /// frame, if the tunnel is up.
    pub fn echo(&mut self) -> Result<(u32, Option<Vec<u8>>), SendError> {
        self.echo_seq = self.echo_seq.wrapping_add(1);
        let packet = packet::build_udp(
            SocketAddrV4::new(self.inner, ECHO_SOURCE_PORT),
            TUNNEL_ECHO,
            &self.echo_seq.to_be_bytes(),
        );
        self.encapsulate(&packet)
            .map(|frame| (self.echo_seq, frame))
    }

    /// Runs WireGuard's timers (rekeying, keepalives, and the clock and
    /// rate-limiter reset `boringtun` keeps there). Call a few times a
    /// second; returns frames to send.
    pub fn tick(&mut self) -> Vec<Vec<u8>> {
        match self.tunn.update_timers(&mut self.buf) {
            TunnResult::WriteToNetwork(message) => vec![frame::data(&self.session, message)],
            _ => Vec::new(),
        }
    }

    /// Processes one datagram from the relay at `relay_ip`.
    pub fn handle(&mut self, datagram: &[u8], relay_ip: Option<IpAddr>) -> Handled {
        let mut handled = Handled::default();
        let message = match Frame::parse(datagram) {
            Ok(Frame::Data { session, message }) if session == self.session => message.to_vec(),
            Ok(Frame::HelloAck {
                session,
                status,
                cookie,
            }) if session == self.session => {
                handled.events.push(match status {
                    // Only the exit answers Ok, and only once a handshake of
                    // ours has confirmed its return path: the tunnel is up,
                    // whatever an OkNewSession that arrived late through a
                    // slower relay said before.
                    HelloStatus::Ok => {
                        self.needs_handshake = false;
                        Event::Accepted {
                            needs_handshake: false,
                        }
                    }
                    HelloStatus::OkNewSession => {
                        self.needs_handshake = true;
                        Event::Accepted {
                            needs_handshake: true,
                        }
                    }
                    HelloStatus::Verify => match cookie {
                        Some(cookie) => {
                            self.cookie = Some(cookie);
                            Event::Verify(cookie)
                        }
                        None => return handled, // malformed; parse requires it
                    },
                    refused => Event::Refused(refused),
                });
                return handled;
            }
            _ => return handled,
        };
        // boringtun only updates its clock and resets its handshake rate
        // limiter in update_timers.
        handled.send.extend(self.tick());
        let incoming = message.first().copied();
        let mut next: &[u8] = &message;
        loop {
            // The source address only matters for the first message (the
            // cookie mechanism under load); draining passes none.
            let src = relay_ip.filter(|_| !next.is_empty());
            match self.tunn.decapsulate(src, next, &mut self.buf) {
                TunnResult::WriteToNetwork(reply) => {
                    // Accepting a handshake response yields the confirming
                    // keepalive.
                    if next.first() == Some(&WG_HANDSHAKE_RESPONSE) {
                        self.needs_handshake = false;
                        handled.authenticated = true;
                        handled.events.push(Event::HandshakeCompleted);
                    }
                    handled.send.push(frame::data(&self.session, reply));
                    next = &[]; // drain queued packets
                }
                TunnResult::WriteToTunnelV4(inner, _) => {
                    handled.authenticated = true;
                    handled.events.push(classify(inner));
                    break;
                }
                TunnResult::WriteToTunnelV6(inner, _) => {
                    handled.authenticated = true;
                    handled.events.push(Event::Packet(inner.to_vec()));
                    break;
                }
                TunnResult::Done => {
                    if !next.is_empty() {
                        match incoming {
                            Some(WG_DATA) => handled.authenticated = true, // a keepalive
                            Some(WG_COOKIE_REPLY) => handled.events.push(Event::CookieReply),
                            _ => {}
                        }
                    }
                    break;
                }
                TunnResult::Err(e) => {
                    tracing::debug!(error = ?e, "tunnel rejected a message");
                    break;
                }
            }
        }
        handled
    }
}

/// An echo reply, or a packet for the adapter.
fn classify(inner: &[u8]) -> Event {
    if let Ok(udp) = packet::parse_udp(inner) {
        if udp.src == TUNNEL_ECHO && udp.dst.port() == ECHO_SOURCE_PORT {
            if let Ok(seq) = <[u8; 4]>::try_from(udp.payload) {
                return Event::Echo(u32::from_be_bytes(seq));
            }
        }
    }
    Event::Packet(inner.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::WireGuardKeys;

    const SESSION: SessionId = [9; 8];

    fn pair() -> (TunnelCore, Tunn) {
        let client = WireGuardKeys::generate().unwrap();
        let exit = WireGuardKeys::generate().unwrap();
        let core = TunnelCore::new(
            SESSION,
            client.secret,
            exit.public,
            Ipv4Addr::new(10, 200, 0, 2),
        )
        .unwrap();
        let server = Tunn::new(exit.secret, client.public, None, None, 7, None);
        (core, server)
    }

    /// The WireGuard message inside a DATA frame.
    fn message(frame: &[u8]) -> &[u8] {
        match Frame::parse(frame).unwrap() {
            Frame::Data { message, .. } => message,
            other => panic!("expected DATA, got {other:?}"),
        }
    }

    #[test]
    fn hello_acks_become_events() {
        let (mut core, _) = pair();
        let events = |core: &mut TunnelCore, frame: Vec<u8>| core.handle(&frame, None).events;
        assert_eq!(
            events(
                &mut core,
                frame::hello_ack(&SESSION, HelloStatus::OkNewSession)
            ),
            [Event::Accepted {
                needs_handshake: true
            }]
        );
        assert_eq!(
            events(
                &mut core,
                frame::hello_ack(&SESSION, HelloStatus::WrongRegion)
            ),
            [Event::Refused(HelloStatus::WrongRegion)]
        );
        // Another session's answers are ignored.
        assert!(events(&mut core, frame::hello_ack(&[1; 8], HelloStatus::Ok)).is_empty());
        // Ok means the exit's return path is confirmed: no handshake is
        // needed, whatever a late OkNewSession said before.
        assert_eq!(
            events(&mut core, frame::hello_ack(&SESSION, HelloStatus::Ok)),
            [Event::Accepted {
                needs_handshake: false
            }]
        );
        assert!(!core.needs_handshake());

        // Verify stores the cookie and the next HELLO carries it, until forgotten.
        let cookie = [5; 16];
        assert_eq!(
            events(&mut core, frame::hello_verify(&SESSION, &cookie)),
            [Event::Verify(cookie)]
        );
        let token = crate::token::Issuer::from_seed(&[3; 32])
            .unwrap()
            .sign(&crate::token::Claims {
                session_id: SESSION,
                client_key: [1; 32],
                exit_id: 1,
                issued_at: 1,
                expires_at: 2,
                game: "g".into(),
                region: "r".into(),
            })
            .unwrap();
        let with_cookie = |core: &TunnelCore| match Frame::parse(&core.hello(&token)) {
            Ok(Frame::Hello { cookie, .. }) => cookie,
            other => panic!("expected HELLO, got {other:?}"),
        };
        assert_eq!(with_cookie(&core), Some(cookie));
        core.forget_cookie();
        assert_eq!(with_cookie(&core), None);
    }

    #[test]
    fn completes_a_handshake_and_carries_packets_both_ways() {
        let (mut core, mut server) = pair();
        let mut buf = vec![0u8; 2048];
        assert!(core.needs_handshake());

        let init = core.handshake_initiation().unwrap();
        let response = match server.decapsulate(None, message(&init), &mut buf) {
            TunnResult::WriteToNetwork(response) => frame::data(&SESSION, response),
            other => panic!("expected a handshake response, got {other:?}"),
        };
        let handled = core.handle(&response, None);
        assert!(handled.events.contains(&Event::HandshakeCompleted));
        assert!(!core.needs_handshake());
        // The confirming keepalive authenticates the client to the exit.
        assert!(handled.authenticated);
        let keepalive = handled.send.last().expect("a keepalive");
        assert!(matches!(
            server.decapsulate(None, message(keepalive), &mut buf),
            TunnResult::Done
        ));

        // Client to exit.
        let packet = packet::build_udp(
            "10.200.0.2:40000".parse().unwrap(),
            "198.51.100.7:27015".parse().unwrap(),
            b"game data",
        );
        let frame = core.encapsulate(&packet).unwrap().expect("tunnel is up");
        match server.decapsulate(None, message(&frame), &mut buf) {
            TunnResult::WriteToTunnelV4(inner, _) => assert_eq!(inner, &packet[..]),
            other => panic!("expected the packet, got {other:?}"),
        }

        // Exit to client: an echo reply and an ordinary packet.
        let (seq, _) = core.echo().unwrap();
        let reply = packet::build_udp(
            TUNNEL_ECHO,
            SocketAddrV4::new(Ipv4Addr::new(10, 200, 0, 2), ECHO_SOURCE_PORT),
            &seq.to_be_bytes(),
        );
        for (inner, expected) in [
            (reply, Event::Echo(seq)),
            (packet.clone(), Event::Packet(packet.clone())),
        ] {
            let frame = match server.encapsulate(&inner, &mut buf) {
                TunnResult::WriteToNetwork(m) => frame::data(&SESSION, m),
                other => panic!("expected a data message, got {other:?}"),
            };
            assert!(core.handle(&frame, None).events.contains(&expected));
        }

        // A replayed copy is dropped (the replay window; Phase 4 multipath).
        let again = server.encapsulate(&packet, &mut buf);
        let TunnResult::WriteToNetwork(m) = again else {
            panic!()
        };
        let frame = frame::data(&SESSION, m);
        assert_eq!(core.handle(&frame, None).events.len(), 1);
        assert!(core.handle(&frame, None).events.is_empty());
    }

    #[test]
    fn refuses_oversize_packets() {
        let (mut core, _) = pair();
        assert_eq!(
            core.encapsulate(&vec![0x45; TUNNEL_MTU + 1]),
            Err(SendError::TooLarge(TUNNEL_MTU + 1))
        );
    }
}
