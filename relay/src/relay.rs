//! A running relay: shared state, the receive loop that dispatches frames to
//! the entry and exit roles, and the maintenance loop.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use boringtun::noise::rate_limiter::RateLimiter;
use pingpath_games::GameDefinition;
use pingpath_tunnel::frame::{self, Frame, FrameError, HelloStatus, MAX_FRAME, SessionId};
use pingpath_tunnel::keys::WireGuardKeys;
use pingpath_tunnel::token::{Claims, TokenError, Verifier, now_unix};
use ring::{digest, hmac};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::config::{Config, Role};
use crate::entry::EntrySession;
use crate::exit::ExitSession;
use crate::metrics::{DropReason, Metrics};
use crate::probe::ProbeState;
use crate::ratelimit::{Limiter, TokenBucket};

/// Handshakes per second, across all sessions, before the exit asks
/// clients for WireGuard cookies (its built-in DoS defence).
const HANDSHAKES_BEFORE_COOKIES: u64 = 500;
const MAINTENANCE_INTERVAL: Duration = Duration::from_millis(100);
const SUMMARY_INTERVAL: Duration = Duration::from_secs(60);
/// Verified tokens remembered, so a replayed HELLO costs a lookup rather
/// than a signature check.
const TOKEN_CACHE_LIMIT: usize = 100_000;

/// Everything a relay needs besides its config file.
pub struct RelayOptions {
    pub config: Config,
    pub games: Vec<GameDefinition>,
    /// The exit's WireGuard private key (exit role).
    pub exit_secret: Option<[u8; 32]>,
    /// For reporting to the control plane.
    pub control_api_key: Option<String>,
}

pub struct Shared {
    pub config: Config,
    pub socket: Arc<UdpSocket>,
    pub metrics: Metrics,
    pub(crate) verifier: Verifier,
    pub(crate) exit_keys: Option<WireGuardKeys>,
    pub(crate) games: HashMap<String, Arc<GameDefinition>>,
    pub(crate) entry: Mutex<HashMap<SessionId, EntrySession>>,
    pub(crate) exit: Mutex<HashMap<SessionId, Arc<ExitSession>>>,
    pub(crate) exits_by_id: HashMap<u32, SocketAddr>,
    exit_addresses: HashSet<SocketAddr>,
    pub(crate) peers_by_address: HashMap<SocketAddr, u32>,
    /// Our own relays' UDP addresses: never a forwarding destination, so a
    /// tunnel can't be nested inside another or pose as a relay.
    pub(crate) infrastructure: HashSet<SocketAddr>,
    pub(crate) handshake_limiter: Arc<RateLimiter>,
    pub(crate) cookie_key: hmac::Key,
    token_cache: Mutex<HashMap<[u8; 32], Claims>>,
    floods: Mutex<HashMap<IpAddr, (TokenBucket, Instant)>>,
    /// HELLOs that carry a valid entry cookie: the sender proved its address,
    /// so a larger budget per network (players behind one CGNAT failing over
    /// together).
    proven_floods: Mutex<HashMap<IpAddr, (TokenBucket, Instant)>>,
    hello_budget: Mutex<TokenBucket>,
    pub(crate) destinations: Mutex<HashMap<SocketAddrV4, (Limiter, Instant)>>,
    pub(crate) probes: Mutex<ProbeState>,
    pub(crate) control_api_key: Option<String>,
    started: Instant,
}

pub struct Relay {
    shared: Arc<Shared>,
    http_addr: Option<SocketAddr>,
    tasks: Vec<JoinHandle<()>>,
}

impl Relay {
    pub async fn start(options: RelayOptions) -> io::Result<Self> {
        let config = options.config;
        // Config::load has validated the file; here only what start() needs.
        let invalid = |e: String| io::Error::new(io::ErrorKind::InvalidInput, e);
        let issuer_keys = config
            .issuer_public_keys
            .iter()
            .map(|k| pingpath_tunnel::keys::decode(k).map_err(|e| invalid(e.to_string())))
            .collect::<io::Result<Vec<_>>>()?;
        let exit_keys = match (config.has_role(Role::Exit), options.exit_secret) {
            (true, Some(secret)) => Some(WireGuardKeys::from_secret(secret)),
            (true, None) => return Err(invalid("the exit role needs a private key".into())),
            (false, _) => None,
        };
        let limiter_key = exit_keys
            .as_ref()
            .map_or_else(|| WireGuardKeys::from_secret([1; 32]).public, |k| k.public);
        let cookie_secret =
            pingpath_tunnel::keys::random_32().map_err(|e| io::Error::other(e.to_string()))?;

        let socket = Arc::new(UdpSocket::bind(config.listen).await?);
        let bound = socket.local_addr()?;
        let exits_by_id: HashMap<u32, SocketAddr> = config
            .relays
            .iter()
            .filter(|p| p.roles.contains(&Role::Exit))
            .map(|p| (p.id, p.address))
            .collect();
        let mut infrastructure: HashSet<SocketAddr> =
            config.relays.iter().map(|p| p.address).collect();
        if !bound.ip().is_unspecified() {
            infrastructure.insert(bound);
        }
        let hellos = config.limits.hellos_per_second;
        let shared = Arc::new(Shared {
            exit_addresses: exits_by_id.values().copied().collect(),
            peers_by_address: config.relays.iter().map(|p| (p.address, p.id)).collect(),
            exits_by_id,
            infrastructure,
            verifier: Verifier::new(issuer_keys),
            exit_keys,
            games: options
                .games
                .into_iter()
                .map(|g| (g.id.clone(), Arc::new(g)))
                .collect(),
            entry: Mutex::new(HashMap::new()),
            exit: Mutex::new(HashMap::new()),
            handshake_limiter: Arc::new(RateLimiter::new(
                &limiter_key,
                // A dead entry moves all its sessions to the same exit at once.
                HANDSHAKES_BEFORE_COOKIES.max(config.limits.max_sessions as u64 / 10),
            )),
            cookie_key: hmac::Key::new(hmac::HMAC_SHA256, &cookie_secret),
            token_cache: Mutex::new(HashMap::new()),
            floods: Mutex::new(HashMap::new()),
            proven_floods: Mutex::new(HashMap::new()),
            hello_budget: Mutex::new(TokenBucket::new(hellos, hellos)),
            destinations: Mutex::new(HashMap::new()),
            probes: Mutex::new(ProbeState::default()),
            control_api_key: options.control_api_key,
            metrics: Metrics::default(),
            socket,
            started: Instant::now(),
            config,
        });

        let mut tasks = vec![
            tokio::spawn(receive_loop(shared.clone())),
            tokio::spawn(maintenance_loop(shared.clone())),
            tokio::spawn(summary_loop(shared.clone())),
        ];
        let has_probe_targets =
            !shared.config.relays.is_empty() || shared.config.has_role(Role::Exit);
        if shared.config.probe.interval_secs > 0 && has_probe_targets {
            tasks.push(tokio::spawn(crate::probe::probe_loop(shared.clone())));
        }
        let http_addr = match shared.config.http {
            Some(addr) => {
                let (bound, task) = crate::http::serve(shared.clone(), addr).await?;
                tasks.push(task);
                Some(bound)
            }
            None => None,
        };
        tracing::info!(
            id = shared.config.id,
            name = %shared.config.name,
            listen = %shared.socket.local_addr()?,
            http = ?http_addr,
            roles = ?shared.config.roles,
            regions = ?shared.config.regions,
            games = shared.games.len(),
            exits = shared.exits_by_id.len(),
            "relay started"
        );
        Ok(Self {
            shared,
            http_addr,
            tasks,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.shared
            .socket
            .local_addr()
            .expect("bound socket has an address")
    }

    pub fn http_addr(&self) -> Option<SocketAddr> {
        self.http_addr
    }

    pub fn metrics(&self) -> &Metrics {
        &self.shared.metrics
    }

    /// The exit's WireGuard public key, as clients need it.
    pub fn exit_public_key(&self) -> Option<boringtun::x25519::PublicKey> {
        self.shared.exit_keys.as_ref().map(|k| k.public)
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    pub fn shutdown(self) {
        tracing::info!(id = self.shared.config.id, "relay stopping");
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// The network a source address belongs to, for per-source limits: a /24
/// for IPv4 and a /64 for IPv6, so one host can't dodge them by varying its
/// address.
fn source_network(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            IpAddr::from([a, b, c, 0])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            IpAddr::from([s[0], s[1], s[2], s[3], 0, 0, 0, 0])
        }
    }
}

impl Shared {
    pub(crate) fn millis(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    pub(crate) async fn send(&self, frame: &[u8], to: SocketAddr) {
        match self.socket.send_to(frame, to).await {
            Ok(_) => {
                self.metrics
                    .frames_forwarded
                    .fetch_add(1, Ordering::Relaxed);
                self.metrics
                    .bytes_forwarded
                    .fetch_add(frame.len() as u64, Ordering::Relaxed);
            }
            Err(e) => {
                tracing::debug!(error = %e, "send failed");
                self.metrics.count_drop(DropReason::SendFailed);
            }
        }
    }

    /// HELLO and PROBE frames are cheap to send and costly to process, so
    /// each source network gets a small budget.
    fn within_handshake_budget(&self, from: IpAddr) -> bool {
        Self::take_from(&self.floods, self.config.limits.handshakes_per_second, from)
    }

    /// The budget for HELLOs whose cookie proves the sender's address: ten
    /// times the normal one.
    fn within_proven_budget(&self, from: IpAddr) -> bool {
        Self::take_from(
            &self.proven_floods,
            self.config.limits.handshakes_per_second.saturating_mul(10),
            from,
        )
    }

    fn take_from(
        table: &Mutex<HashMap<IpAddr, (TokenBucket, Instant)>>,
        rate: u64,
        from: IpAddr,
    ) -> bool {
        let mut floods = table.lock().expect("flood table lock");
        let (bucket, last) = floods
            .entry(source_network(from))
            .or_insert_with(|| (TokenBucket::new(rate, rate), Instant::now()));
        *last = Instant::now();
        bucket.take(1)
    }

    /// Verifies a token, remembering ones that verified so replays are cheap.
    fn verify_token(&self, token: &[u8]) -> Result<Claims, TokenError> {
        let now = now_unix();
        let hash: [u8; 32] = digest::digest(&digest::SHA256, token)
            .as_ref()
            .try_into()
            .expect("SHA-256 is 32 bytes");
        if let Some(claims) = self
            .token_cache
            .lock()
            .expect("token cache lock")
            .get(&hash)
        {
            if now
                >= claims
                    .expires_at
                    .saturating_add(pingpath_tunnel::token::CLOCK_SKEW_SECS)
            {
                return Err(TokenError::Expired);
            }
            return Ok(claims.clone());
        }
        let claims = self.verifier.verify(token, now)?;
        let mut cache = self.token_cache.lock().expect("token cache lock");
        if cache.len() < TOKEN_CACHE_LIMIT {
            cache.insert(hash, claims.clone());
        }
        Ok(claims)
    }

    /// Handles one datagram and returns the frames to send in reply or on.
    async fn handle(
        self: &Arc<Self>,
        datagram: &[u8],
        from: SocketAddr,
    ) -> Vec<(Vec<u8>, SocketAddr)> {
        if datagram.len() > MAX_FRAME {
            self.metrics.count_drop(DropReason::Oversize);
            return Vec::new();
        }
        let frame = match Frame::parse(datagram) {
            Ok(frame) => frame,
            Err(FrameError::UnknownType(_)) => {
                self.metrics.count_drop(DropReason::UnknownFrameType);
                return Vec::new();
            }
            Err(_) => {
                self.metrics.count_drop(DropReason::MalformedFrame);
                return Vec::new();
            }
        };
        let is_exit = self.config.has_role(Role::Exit);
        let is_entry = self.config.has_role(Role::Entry);
        match frame {
            Frame::Probe { body } => {
                if !self.within_handshake_budget(from.ip()) {
                    self.metrics.count_drop(DropReason::ProbeFlood);
                    return Vec::new();
                }
                self.metrics.probes_answered.fetch_add(1, Ordering::Relaxed);
                vec![(frame::probe_reply(body), from)]
            }
            Frame::ProbeReply { body } => {
                self.probe_reply(body, from);
                Vec::new()
            }
            Frame::Hello {
                session,
                token,
                cookie,
            } => {
                if self.exit_addresses.contains(&from) {
                    // Exits never send HELLO.
                    self.metrics.count_drop(DropReason::MalformedFrame);
                    return Vec::new();
                }
                // HELLOs an entry forwards come from the entry's address, so
                // budgets per source network would throttle every client of
                // that entry together (all of them at once when another
                // entry dies). Relays in our own config are trusted for this.
                let from_peer = self.peers_by_address.contains_key(&from);
                // A valid cookie proves the sender receives at its address;
                // checking it is a cheap HMAC, done before the signature.
                let proven =
                    is_entry && cookie.is_some_and(|c| self.cookie_is_valid(&session, from, &c));
                let within = from_peer
                    || if proven {
                        self.within_proven_budget(from.ip())
                    } else {
                        self.within_handshake_budget(from.ip())
                            && self.hello_budget.lock().expect("hello budget lock").take(1)
                    };
                if !within {
                    self.metrics.count_drop(DropReason::HelloFlood);
                    return Vec::new();
                }
                let claims = match self.verify_token(token) {
                    Ok(claims) => claims,
                    Err(TokenError::Expired | TokenError::NotYetValid) => {
                        return self.refuse(
                            &session,
                            DropReason::ExpiredToken,
                            HelloStatus::Expired,
                            from,
                        );
                    }
                    // Unsigned or forged: never answered, so garbage from a
                    // spoofed address gets nothing sent anywhere.
                    Err(_) => {
                        self.metrics.count_drop(DropReason::BadToken);
                        return Vec::new();
                    }
                };
                if claims.session_id != session {
                    self.metrics.count_drop(DropReason::BadToken);
                    return Vec::new();
                }
                if is_exit && claims.exit_id == self.config.id {
                    self.exit_hello(&claims, from)
                } else if is_entry {
                    self.entry_hello(&claims, datagram, cookie, from)
                        .into_iter()
                        .collect()
                } else {
                    self.refuse(
                        &session,
                        DropReason::WrongExit,
                        HelloStatus::WrongExit,
                        from,
                    )
                }
            }
            Frame::HelloAck { session, .. } => {
                if is_entry && self.exit_addresses.contains(&from) {
                    self.entry_from_exit(&session, datagram, from)
                        .into_iter()
                        .collect()
                } else {
                    self.metrics.count_drop(DropReason::UnknownSession);
                    Vec::new()
                }
            }
            Frame::Data { session, message } => {
                if is_exit {
                    if let Some(exit_session) = self.exit_session(&session) {
                        return self.exit_data(exit_session, message, from).await;
                    }
                }
                if is_entry {
                    if self.exit_addresses.contains(&from) {
                        return self
                            .entry_from_exit(&session, datagram, from)
                            .into_iter()
                            .collect();
                    }
                    return self
                        .entry_from_client(&session, datagram, from)
                        .into_iter()
                        .collect();
                }
                self.metrics.count_drop(DropReason::UnknownSession);
                Vec::new()
            }
        }
    }

    fn refuse(
        &self,
        session: &SessionId,
        reason: DropReason,
        status: HelloStatus,
        to: SocketAddr,
    ) -> Vec<(Vec<u8>, SocketAddr)> {
        self.metrics.count_drop(reason);
        tracing::debug!(session = %frame::session_hex(session), ?status, "HELLO refused");
        vec![(frame::hello_ack(session, status), to)]
    }
}

async fn receive_loop(shared: Arc<Shared>) {
    // Larger than any frame we accept, so an oversize datagram is noticed
    // rather than silently truncated.
    let mut buf = vec![0u8; 65_535];
    loop {
        let (n, from) = match shared.socket.recv_from(&mut buf).await {
            Ok(received) => received,
            Err(e) => {
                // On Linux, ICMP errors for earlier sends surface here; keep going.
                tracing::debug!(error = %e, "receive error");
                continue;
            }
        };
        let started = Instant::now();
        shared
            .metrics
            .frames_received
            .fetch_add(1, Ordering::Relaxed);
        shared
            .metrics
            .bytes_received
            .fetch_add(n as u64, Ordering::Relaxed);
        let outgoing = shared.handle(&buf[..n], from).await;
        for (frame, to) in &outgoing {
            shared.send(frame, *to).await;
        }
        if !outgoing.is_empty() {
            shared.metrics.record_processing(started.elapsed());
        }
    }
}

async fn maintenance_loop(shared: Arc<Shared>) {
    let mut interval = tokio::time::interval(MAINTENANCE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_second = Instant::now();
    loop {
        interval.tick().await;
        let now = now_unix();
        shared.exit_maintenance(now).await;
        shared.expire_entry_sessions(now);
        if last_second.elapsed() >= Duration::from_secs(1) {
            last_second = Instant::now();
            // boringtun's shared handshake counter is per second.
            shared.handshake_limiter.reset_count();
            for table in [&shared.floods, &shared.proven_floods] {
                table
                    .lock()
                    .expect("flood table lock")
                    .retain(|_, (_, last)| last.elapsed() < Duration::from_secs(60));
            }
            shared
                .token_cache
                .lock()
                .expect("token cache lock")
                .retain(|_, claims| {
                    now < claims
                        .expires_at
                        .saturating_add(pingpath_tunnel::token::CLOCK_SKEW_SECS)
                });
        }
    }
}

/// Logs a one-line summary every minute (PLAN.md section 9: drops are
/// counted, not logged one by one).
async fn summary_loop(shared: Arc<Shared>) {
    let mut interval = tokio::time::interval(SUMMARY_INTERVAL);
    interval.tick().await;
    loop {
        interval.tick().await;
        let m = &shared.metrics;
        let drops: Vec<String> = m
            .drop_summary()
            .iter()
            .map(|(reason, n)| format!("{reason}={n}"))
            .collect();
        tracing::info!(
            entry_sessions = m.entry_sessions.load(Ordering::Relaxed),
            exit_sessions = m.exit_sessions.load(Ordering::Relaxed),
            flows = m.flows.load(Ordering::Relaxed),
            frames_received = m.frames_received.load(Ordering::Relaxed),
            frames_forwarded = m.frames_forwarded.load(Ordering::Relaxed),
            drops = %drops.join(" "),
            "relay summary"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_networks_group_neighbours() {
        assert_eq!(
            source_network("203.0.113.77".parse().unwrap()),
            "203.0.113.0".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            source_network("2001:db8:1:2:3:4:5:6".parse().unwrap()),
            "2001:db8:1:2::".parse::<IpAddr>().unwrap()
        );
    }
}
