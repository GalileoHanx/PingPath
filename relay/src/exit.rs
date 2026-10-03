//! The exit role: terminates each session's WireGuard tunnel, forwards UDP
//! to the session's game servers only (docs/decisions/005-exit-egress.md)
//! and carries the replies back.

use std::collections::HashMap;
use std::net::{SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use boringtun::noise::errors::WireGuardError;
use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::PublicKey;
use pingpath_games::GameDefinition;
use pingpath_tunnel::frame::{self, HelloStatus, SessionId, session_hex};
use pingpath_tunnel::packet::{self, MAX_TUNNEL_UDP_PAYLOAD, PacketError, TUNNEL_ECHO};
use pingpath_tunnel::token::{CLOCK_SKEW_SECS, Claims, now_unix};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::metrics::DropReason;
use crate::ratelimit::{Limiter, TokenBucket};
use crate::relay::Shared;

/// WireGuard message types (the first byte of a message).
const WG_HANDSHAKE_INIT: u8 = 1;
const WG_HANDSHAKE_RESPONSE: u8 = 2;
const WG_COOKIE_REPLY: u8 = 3;
const WG_DATA: u8 = 4;
/// Room for any decrypted message (DATA frames are capped at the tunnel MTU).
const BUFFER_LEN: usize = 65_535 + 64;

type FlowKey = (SocketAddrV4, SocketAddrV4);

pub struct ExitSession {
    pub id: SessionId,
    pub game: Arc<GameDefinition>,
    pub region: String,
    pub client_key: [u8; 32],
    pub expires_at: AtomicU64,
    created: Instant,
    tunnel: Mutex<TunnelState>,
    up: Mutex<Limiter>,
    down: Mutex<Limiter>,
    new_flows: Mutex<TokenBucket>,
    flows: Mutex<HashMap<FlowKey, Flow>>,
}

/// The WireGuard state and where replies go, kept under one lock.
#[derive(Default)]
struct TunnelState {
    /// Created on the client's first handshake initiation, not on HELLO.
    tunn: Option<Tunn>,
    /// Where replies go: an address that completed a handshake and then
    /// sent data under the keys it produced.
    path: Option<ReturnPath>,
    /// An address that sent a handshake initiation we answered, and the
    /// receiver index its data must carry to confirm it.
    pending: Option<(SocketAddr, u32)>,
}

struct ReturnPath {
    addr: SocketAddr,
    last_authenticated: Instant,
}

struct Flow {
    socket: Arc<UdpSocket>,
    /// Milliseconds since the relay started.
    last_used: Arc<AtomicU64>,
    opened: u64,
    answered: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl Drop for Flow {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ExitSession {
    pub fn flow_count(&self) -> usize {
        self.flows.lock().expect("flows lock").len()
    }
}

/// Frames to send after processing one datagram, in order.
pub type Outgoing = Vec<(Vec<u8>, SocketAddr)>;

fn wg_kind(message: &[u8]) -> Option<u8> {
    message.first().copied()
}

/// The receiver index in a WireGuard data message, or the sender index in a
/// handshake response (both at bytes 4..8).
fn wg_index(message: &[u8]) -> Option<u32> {
    message
        .get(4..8)
        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
}

impl Shared {
    /// A HELLO with a verified token naming this relay as the exit.
    pub(crate) fn exit_hello(&self, claims: &Claims, from: SocketAddr) -> Outgoing {
        let session = &claims.session_id;
        let reply = |status| vec![(frame::hello_ack(session, status), from)];
        if !self.config.regions.contains(&claims.region) {
            self.metrics.count_drop(DropReason::WrongRegion);
            return reply(HelloStatus::WrongRegion);
        }
        let Some(game) = self.games.get(&claims.game).filter(|g| {
            g.status.routing_policy() != pingpath_games::RoutingPolicy::Refuse
                && g.region(&claims.region).is_some()
        }) else {
            self.metrics.count_drop(DropReason::GameNotRoutable);
            tracing::info!(session = %session_hex(session), game = %claims.game, region = %claims.region, "refused: game not routable here");
            return reply(HelloStatus::GameNotRoutable);
        };
        if self.exit_keys.is_none() {
            self.metrics.count_drop(DropReason::WrongExit);
            return reply(HelloStatus::WrongExit);
        }

        let mut sessions = self.exit.lock().expect("exit table lock");
        if let Some(existing) = sessions.get(session) {
            if existing.client_key != claims.client_key
                || existing.game.id != claims.game
                || existing.region != claims.region
            {
                // Same session ID, different grant: not a renewal.
                self.metrics.count_drop(DropReason::BadToken);
                return reply(HelloStatus::BadToken);
            }
            existing
                .expires_at
                .fetch_max(claims.expires_at, Ordering::Relaxed);
            let established = existing.tunnel.lock().expect("tunnel lock").path.is_some();
            tracing::info!(session = %session_hex(session), "exit session renewed");
            return reply(if established {
                HelloStatus::Ok
            } else {
                HelloStatus::OkNewSession
            });
        }
        if sessions.values().any(|s| s.client_key == claims.client_key) {
            // One WireGuard key per session: a reused key would let a
            // handshake recorded in one session replay into another.
            self.metrics.count_drop(DropReason::BadToken);
            return reply(HelloStatus::BadToken);
        }
        if sessions.len() >= self.config.limits.max_sessions {
            self.metrics.count_drop(DropReason::RelayFull);
            return reply(HelloStatus::RelayFull);
        }
        let limits = &self.config.limits;
        sessions.insert(
            *session,
            Arc::new(ExitSession {
                id: *session,
                game: game.clone(),
                region: claims.region.clone(),
                client_key: claims.client_key,
                expires_at: AtomicU64::new(claims.expires_at),
                created: Instant::now(),
                tunnel: Mutex::new(TunnelState::default()),
                up: Mutex::new(self.limiter()),
                down: Mutex::new(self.limiter()),
                new_flows: Mutex::new(TokenBucket::new(
                    limits.new_flows_per_second,
                    limits.max_flows_per_session as u64,
                )),
                flows: Mutex::new(HashMap::new()),
            }),
        );
        self.metrics.exit_sessions.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .sessions_started
            .fetch_add(1, Ordering::Relaxed);
        tracing::info!(session = %session_hex(session), game = %claims.game, region = %claims.region, "exit session started");
        reply(HelloStatus::OkNewSession)
    }

    pub(crate) fn exit_session(&self, id: &SessionId) -> Option<Arc<ExitSession>> {
        self.exit.lock().expect("exit table lock").get(id).cloned()
    }

    /// How an address reached us, for logs: a relay ID, never a client address.
    fn via(&self, addr: SocketAddr) -> String {
        match self.peers_by_address.get(&addr) {
            Some(id) => format!("relay {id}"),
            None => "direct".to_string(),
        }
    }

    /// DATA for one of this exit's sessions. Returns frames to send.
    pub(crate) async fn exit_data(
        self: &Arc<Self>,
        session: Arc<ExitSession>,
        message: &[u8],
        from: SocketAddr,
    ) -> Outgoing {
        if now_unix()
            >= session
                .expires_at
                .load(Ordering::Relaxed)
                .saturating_add(CLOCK_SKEW_SECS)
        {
            self.metrics.count_drop(DropReason::ExpiredToken);
            return Vec::new();
        }
        let incoming = wg_kind(message);
        let mut to_sender: Vec<Vec<u8>> = Vec::new();
        let mut to_path: Vec<Vec<u8>> = Vec::new();
        let mut inner_packets: Vec<Vec<u8>> = Vec::new();
        {
            let mut state = session.tunnel.lock().expect("tunnel lock");
            if state.tunn.is_none() {
                if incoming != Some(WG_HANDSHAKE_INIT) {
                    self.metrics.count_drop(DropReason::UnknownSession);
                    return Vec::new();
                }
                let keys = self.exit_keys.as_ref().expect("exit role has keys");
                let index =
                    u32::from_le_bytes(session.id[..4].try_into().expect("4 bytes")) & 0x00ff_ffff;
                state.tunn = Some(Tunn::new(
                    keys.secret.clone(),
                    PublicKey::from(session.client_key),
                    None,
                    None,
                    index,
                    Some(self.handshake_limiter.clone()),
                ));
            }
            let mut buf = vec![0u8; BUFFER_LEN];
            let tunn = state.tunn.as_mut().expect("created above");
            let mut authenticated_data = false;
            match tunn.decapsulate(Some(from.ip()), message, &mut buf) {
                TunnResult::WriteToNetwork(reply) => match wg_kind(reply) {
                    // We accepted the client's handshake: answer the address it
                    // came from, and let that address become the return path
                    // once it sends data under the new keys.
                    Some(WG_HANDSHAKE_RESPONSE) => {
                        state.pending = wg_index(reply).map(|index| (from, index));
                        to_sender.push(frame::data(&session.id, reply));
                    }
                    // Under load: a cookie for whoever asked. Proves nothing.
                    Some(WG_COOKIE_REPLY) => to_sender.push(frame::data(&session.id, reply)),
                    _ => {}
                },
                TunnResult::WriteToTunnelV4(inner, _) => {
                    authenticated_data = true;
                    inner_packets.push(inner.to_vec());
                }
                TunnResult::WriteToTunnelV6(..) => {
                    authenticated_data = true;
                    self.metrics.count_drop(DropReason::NotIpv4);
                }
                // A keepalive: decrypted, empty.
                TunnResult::Done => authenticated_data = incoming == Some(WG_DATA),
                // Checked before decryption, so a forged copy causes it too.
                TunnResult::Err(WireGuardError::DuplicateCounter) => {
                    self.metrics.count_drop(DropReason::Duplicate)
                }
                TunnResult::Err(e) => {
                    tracing::debug!(session = %session_hex(&session.id), error = ?e, "tunnel rejected a message");
                    self.metrics.count_drop(DropReason::TunnelRejected);
                }
            }

            if authenticated_data {
                let index = wg_index(message);
                if state
                    .pending
                    .is_some_and(|(addr, pending)| addr == from && Some(pending) == index)
                {
                    tracing::info!(session = %session_hex(&session.id), via = %self.via(from), "return path confirmed");
                    state.path = Some(ReturnPath {
                        addr: from,
                        last_authenticated: Instant::now(),
                    });
                    state.pending = None;
                } else if let Some(path) = state.path.as_mut().filter(|p| p.addr == from) {
                    path.last_authenticated = Instant::now();
                }
                // Anything queued while the handshake was pending.
                let tunn = state.tunn.as_mut().expect("created above");
                while let TunnResult::WriteToNetwork(queued) = tunn.decapsulate(None, &[], &mut buf)
                {
                    if wg_kind(queued) != Some(WG_HANDSHAKE_INIT) {
                        to_path.push(frame::data(&session.id, queued));
                    }
                }
            }
        }

        let mut outgoing: Outgoing = to_sender.into_iter().map(|f| (f, from)).collect();
        for inner in inner_packets {
            if let Some(frame) = self.forward_inner(&session, &inner).await {
                to_path.push(frame);
            }
        }
        if !to_path.is_empty() {
            match self.path_target(&session) {
                Some(path) => outgoing.extend(to_path.into_iter().map(|f| (f, path))),
                None => self.metrics.count_drop(DropReason::StalePath),
            }
        }
        outgoing
    }

    /// The return path, if it has authenticated recently enough to keep
    /// sending to it.
    fn path_target(&self, session: &ExitSession) -> Option<SocketAddr> {
        let stale_after = Duration::from_secs(self.config.limits.stale_path_secs);
        let state = session.tunnel.lock().expect("tunnel lock");
        state
            .path
            .as_ref()
            .filter(|p| p.last_authenticated.elapsed() < stale_after)
            .map(|p| p.addr)
    }

    /// Encrypts an IPv4 packet for the client. `None` while no session keys
    /// exist; never a handshake initiation (the exit only answers).
    fn encrypt_for_client(&self, session: &ExitSession, inner: &[u8]) -> Option<Vec<u8>> {
        let mut state = session.tunnel.lock().expect("tunnel lock");
        let tunn = state.tunn.as_mut()?;
        let mut out = vec![0u8; inner.len() + 64 + 148];
        match tunn.encapsulate(inner, &mut out) {
            TunnResult::WriteToNetwork(message) if wg_kind(message) == Some(WG_DATA) => {
                Some(frame::data(&session.id, message))
            }
            _ => None,
        }
    }

    /// Checks one decrypted IPv4 packet and sends its UDP payload to the
    /// game server. Returns a frame for the client when the exit answers
    /// itself (the in-tunnel echo).
    async fn forward_inner(
        self: &Arc<Self>,
        session: &Arc<ExitSession>,
        inner: &[u8],
    ) -> Option<Vec<u8>> {
        let udp = match packet::parse_udp(inner) {
            Ok(udp) => udp,
            Err(e) => {
                self.metrics.count_drop(match e {
                    PacketError::NotIpv4(_) => DropReason::NotIpv4,
                    PacketError::NotUdp(_) => DropReason::NotUdp,
                    PacketError::Fragment => DropReason::Fragment,
                    PacketError::Malformed => DropReason::MalformedPacket,
                });
                return None;
            }
        };
        // Session limits are charged only for packets that authenticated, so
        // junk sent with a stolen session ID can't use up a player's budget.
        if !session.up.lock().expect("limiter lock").allow(inner.len()) {
            self.metrics.count_drop(DropReason::RateLimited);
            return None;
        }
        if udp.dst == TUNNEL_ECHO {
            let reply = packet::build_udp(TUNNEL_ECHO, udp.src, udp.payload);
            return self.encrypt_for_client(session, &reply);
        }
        if self.infrastructure.contains(&SocketAddr::V4(udp.dst)) {
            self.metrics.count_drop(DropReason::DeniedInfrastructure);
            return None;
        }
        if !session
            .game
            .allows_udp(&session.region, SocketAddr::V4(udp.dst))
        {
            self.metrics.count_drop(DropReason::NotAllowlisted);
            tracing::debug!(session = %session_hex(&session.id), dst = %udp.dst, game = %session.game.id, region = %session.region, "destination not allowlisted");
            return None;
        }
        if !self.destination_allows(udp.dst, udp.payload.len()) {
            self.metrics.count_drop(DropReason::DestinationLimited);
            return None;
        }
        let socket = match self.flow_socket(session, udp.src, udp.dst).await {
            Ok(socket) => socket,
            Err(reason) => {
                self.metrics.count_drop(reason);
                return None;
            }
        };
        match socket.send(udp.payload).await {
            Ok(_) => {
                self.metrics
                    .frames_forwarded
                    .fetch_add(1, Ordering::Relaxed);
                self.metrics
                    .bytes_forwarded
                    .fetch_add(udp.payload.len() as u64, Ordering::Relaxed);
            }
            Err(e) => {
                tracing::debug!(session = %session_hex(&session.id), dst = %udp.dst, error = %e, "send to game server failed");
                self.metrics.count_drop(DropReason::SendFailed);
            }
        }
        None
    }

    /// Relay-wide limit on traffic to one game server, so many sessions
    /// together can't flood it (rule 10).
    fn destination_allows(&self, dst: SocketAddrV4, len: usize) -> bool {
        let limits = &self.config.limits;
        let mut destinations = self.destinations.lock().expect("destinations lock");
        let (limiter, last) = destinations.entry(dst).or_insert_with(|| {
            (
                Limiter::new(
                    limits.destination_packets_per_second,
                    limits.destination_bits_per_second,
                ),
                Instant::now(),
            )
        });
        *last = Instant::now();
        limiter.allow(len)
    }

    /// The flow's socket, opening the flow (and its reply task) if new.
    async fn flow_socket(
        self: &Arc<Self>,
        session: &Arc<ExitSession>,
        src: SocketAddrV4,
        dst: SocketAddrV4,
    ) -> Result<Arc<UdpSocket>, DropReason> {
        let now = self.millis();
        if let Some(flow) = session.flows.lock().expect("flows lock").get(&(src, dst)) {
            flow.last_used.store(now, Ordering::Relaxed);
            return Ok(flow.socket.clone());
        }
        let limits = &self.config.limits;
        if session.flow_count() >= limits.max_flows_per_session
            || self.metrics.flows.load(Ordering::Relaxed) >= limits.max_flows
        {
            return Err(DropReason::FlowLimit);
        }
        if !session.new_flows.lock().expect("flow rate lock").take(1) {
            return Err(DropReason::FlowRate);
        }
        let socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|_| DropReason::SendFailed)?;
        socket
            .connect(dst)
            .await
            .map_err(|_| DropReason::SendFailed)?;
        let socket = Arc::new(socket);
        let last_used = Arc::new(AtomicU64::new(now));
        let answered = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(flow_replies(
            self.clone(),
            Arc::downgrade(session),
            socket.clone(),
            last_used.clone(),
            answered.clone(),
            src,
            dst,
        ));

        let mut flows = session.flows.lock().expect("flows lock");
        if let Some(existing) = flows.get(&(src, dst)) {
            // Another packet opened it meanwhile; use that one.
            return Ok(existing.socket.clone());
        }
        flows.insert(
            (src, dst),
            Flow {
                socket: socket.clone(),
                last_used,
                opened: now,
                answered,
                task,
            },
        );
        self.metrics.flows.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(session = %session_hex(&session.id), dst = %dst, "flow opened");
        Ok(socket)
    }

    /// WireGuard timers for every exit session; ends expired sessions and
    /// closes idle flows. Called several times a second.
    pub(crate) async fn exit_maintenance(&self, now_unix: u64) {
        let sessions: Vec<Arc<ExitSession>> = self
            .exit
            .lock()
            .expect("exit table lock")
            .values()
            .cloned()
            .collect();
        let limits = &self.config.limits;
        let idle_after = limits.flow_idle().as_millis() as u64;
        let unanswered_after = limits.unanswered_flow_secs * 1000;
        let pending_for = Duration::from_secs(limits.pending_session_secs);
        let now_ms = self.millis();
        let mut ended = Vec::new();
        for session in sessions {
            if now_unix
                >= session
                    .expires_at
                    .load(Ordering::Relaxed)
                    .saturating_add(CLOCK_SKEW_SECS)
            {
                ended.push((session.id, "token expired"));
                continue;
            }
            let timer_frame = {
                let mut state = session.tunnel.lock().expect("tunnel lock");
                match state.tunn.as_mut() {
                    None if session.created.elapsed() >= pending_for => {
                        ended.push((session.id, "no handshake"));
                        continue;
                    }
                    None => None,
                    Some(tunn) => {
                        let mut buf = vec![0u8; 2048];
                        match tunn.update_timers(&mut buf) {
                            // Keepalives only: the exit never starts a handshake.
                            TunnResult::WriteToNetwork(message)
                                if wg_kind(message) == Some(WG_DATA) =>
                            {
                                Some(frame::data(&session.id, message))
                            }
                            _ => None,
                        }
                    }
                }
            };
            if let (Some(frame), Some(path)) = (timer_frame, self.path_target(&session)) {
                self.send(&frame, path).await;
            }
            let closed = {
                let mut flows = session.flows.lock().expect("flows lock");
                let before = flows.len();
                flows.retain(|_, f| {
                    let idle =
                        now_ms.saturating_sub(f.last_used.load(Ordering::Relaxed)) >= idle_after;
                    let unanswered = !f.answered.load(Ordering::Relaxed)
                        && now_ms.saturating_sub(f.opened) >= unanswered_after;
                    !(idle || unanswered)
                });
                before - flows.len()
            };
            if closed > 0 {
                self.metrics
                    .flows
                    .fetch_sub(closed as i64, Ordering::Relaxed);
            }
        }
        if !ended.is_empty() {
            let mut table = self.exit.lock().expect("exit table lock");
            for (id, reason) in ended {
                if let Some(session) = table.remove(&id) {
                    let flows = session.flow_count() as i64;
                    session.flows.lock().expect("flows lock").clear();
                    self.metrics.flows.fetch_sub(flows, Ordering::Relaxed);
                    self.metrics.exit_sessions.fetch_sub(1, Ordering::Relaxed);
                    self.metrics.sessions_ended.fetch_add(1, Ordering::Relaxed);
                    tracing::info!(session = %session_hex(&id), reason, "exit session ended");
                }
            }
        }
        let mut destinations = self.destinations.lock().expect("destinations lock");
        destinations.retain(|_, (_, last)| last.elapsed() < Duration::from_secs(60));
    }
}

/// Carries a flow's replies from the game server back through the tunnel.
async fn flow_replies(
    shared: Arc<Shared>,
    session: Weak<ExitSession>,
    socket: Arc<UdpSocket>,
    last_used: Arc<AtomicU64>,
    answered: Arc<AtomicBool>,
    client: SocketAddrV4,
    server: SocketAddrV4,
) {
    // One byte more than the tunnel carries, so oversize replies are seen.
    let mut buf = vec![0u8; MAX_TUNNEL_UDP_PAYLOAD + 1];
    loop {
        let n = match socket.recv(&mut buf).await {
            Ok(n) => n,
            // ICMP errors surface as receive errors on a connected socket;
            // pause briefly so a persistent error can't spin the CPU.
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
        };
        let Some(session) = session.upgrade() else {
            return;
        };
        last_used.store(shared.millis(), Ordering::Relaxed);
        answered.store(true, Ordering::Relaxed);
        if n > MAX_TUNNEL_UDP_PAYLOAD {
            shared.metrics.count_drop(DropReason::ReplyTooBig);
            continue;
        }
        if !session.down.lock().expect("limiter lock").allow(n) {
            shared.metrics.count_drop(DropReason::RateLimited);
            continue;
        }
        let inner = packet::build_udp(server, client, &buf[..n]);
        match (
            shared.encrypt_for_client(&session, &inner),
            shared.path_target(&session),
        ) {
            (Some(frame), Some(path)) => shared.send(&frame, path).await,
            (_, None) => shared.metrics.count_drop(DropReason::StalePath),
            (None, _) => shared.metrics.count_drop(DropReason::SendFailed),
        }
    }
}
