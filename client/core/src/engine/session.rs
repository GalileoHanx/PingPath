//! An optimized session's data loop (docs/decisions/009-path-selection-and-failover.md):
//! packets between the adapter and the tunnel, the watchdog, failover to a
//! standby with the same exit, renewal, and the live graph's samples. It
//! ends by itself only through a fallback, after removing the game routes.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pingpath_games::Region;
use pingpath_tunnel::core::{Event, TunnelCore};
use pingpath_tunnel::frame::HelloStatus;
use pingpath_tunnel::packet;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};

use super::guard::Guard;
use super::link::{Hop, Link};
use super::types::{FallbackCause, Sample};
use crate::config::ClientConfig;
use crate::control::{ControlClient, ControlError, Grant};
use crate::filter::{PacketFilter, StrayCounts};
use crate::measure::DirectProbe;
use crate::net::{NextHop, PacketChannels, Platform, Route};
use crate::stats::Window;

/// How often standby entries' cookies are refreshed (they last 30-60 s).
const STANDBY_REFRESH: Duration = Duration::from_secs(20);
/// A game flow that had answers and gets none for this long, while the
/// path itself answers, means the exit stopped delivering it.
const FLOW_SILENCE: Duration = Duration::from_millis(2500);
/// Flows idle this long are forgotten.
const FLOW_IDLE: Duration = Duration::from_secs(30);
/// The share of the failover budget the direct check may take.
const DIRECT_CHECK_SHARE: f64 = 0.8;

/// How much longer the relays are tried when the direct route is down too.
const OUTAGE_EXTENSION: Duration = Duration::from_millis(1500);

/// Blocked game traffic is logged at most this often.
const BLOCKED_LOG_EVERY: Duration = Duration::from_secs(30);

/// At most this many standbys are tried at once.
const PARALLEL_STANDBYS: usize = 3;
/// Retransmit floor during failover.
const MIN_RETRANSMIT: Duration = Duration::from_millis(100);
/// While a renewed token isn't acknowledged by the exit, resend HELLO this often.
const RENEW_HELLO_EVERY: Duration = Duration::from_secs(1);

/// Asks the running session to do something.
#[derive(Debug)]
pub enum Control {
    /// Turn Optimize off (game routes first).
    Stop,
    /// The PC is going to sleep: fall back cleanly.
    Suspend,
    /// Test hook: block the loop, as a hang would.
    #[doc(hidden)]
    Freeze(Duration),
}

/// What the session tells the engine while it runs.
#[derive(Debug)]
pub enum Report {
    Sample(Sample),
    FailingOver,
    /// Traffic moved to another hop with the same exit.
    PathChanged {
        hop: Hop,
        after_ms: u64,
    },
    /// The exit had lost the session: the game server sees new ports.
    AddressChanged,
    Renewed {
        expires_at: u64,
    },
    /// Game traffic the exit can't carry is being dropped.
    GameTrafficBlocked {
        packets: u64,
    },
    Carried(u64),
}

#[derive(Debug)]
pub enum EndReason {
    Stopped,
    FellBack(FallbackCause),
}

/// Handed back to the engine when the session ends.
pub struct Ended {
    pub channels: PacketChannels,
    pub reason: EndReason,
}

pub struct Params {
    pub core: TunnelCore,
    pub link: Link,
    pub current: Hop,
    pub standbys: Vec<Hop>,
    pub token: Vec<u8>,
    pub grant: Grant,
    pub renew_secret: String,
    pub closer: Closer,
    pub control: Option<ControlClient>,
    pub filter: PacketFilter,
    pub channels: PacketChannels,
    pub platform: Arc<dyn Platform>,
    pub game_routes: Vec<Route>,
    pub host_routes: Vec<Route>,
    pub exit_to_region_ms: f64,
    pub path_rtt_ms: f64,
    pub config: ClientConfig,
    pub strays: Arc<StrayCounts>,
    pub reports: mpsc::Sender<Report>,
    pub guard: Arc<Guard>,
    pub direct: Arc<dyn DirectProbe>,
    pub region: Region,
}

/// Closes the session at the control plane when dropped: the player
/// turned it off, it fell back, or it never got going. Its place in the
/// network's limits is freed at once instead of when the token runs out.
/// Best effort, on a thread of its own, so nothing waits for it.
pub struct Closer(Option<(ControlClient, String, String)>);

impl Closer {
    pub fn new(control: Option<ControlClient>, grant: &Grant) -> Self {
        Self(
            control
                .zip(grant.renew_secret.clone())
                .map(|(control, secret)| (control, grant.session_id.clone(), secret)),
        )
    }
}

impl Drop for Closer {
    fn drop(&mut self) {
        if let Some((control, session, secret)) = self.0.take() {
            std::thread::spawn(move || match control.close(&session, &secret) {
                Ok(()) => tracing::info!(%session, "session closed at the control plane"),
                Err(error) => tracing::debug!(%session, %error, "could not close the session"),
            });
        }
    }
}

struct Flow {
    last_out: Instant,
    last_in: Option<Instant>,
}

struct Renewal {
    /// How long the current token lasts.
    lifetime: Duration,
    /// The next renewal attempt.
    next_attempt: Instant,
    task: Option<tokio::task::JoinHandle<Result<Grant, ControlError>>>,
    /// A renewed token the exit hasn't acknowledged yet, and when HELLO was
    /// last sent with it.
    unacked: Option<Instant>,
    /// No more renewals: fall back when the token runs out.
    final_expiry: bool,
    /// When the token the exit holds runs out. A renewed token counts only
    /// once the exit acknowledges it (the exit may refuse it, say with game
    /// definitions that no longer route the game), so until then the old
    /// deadline stands.
    deadline: Instant,
    /// The renewed token's deadline, waiting for the exit's acknowledgement.
    pending_deadline: Option<Instant>,
}

struct Standby {
    hop: Hop,
    last_refresh: Option<Instant>,
}

pub struct Session {
    p: Params,
    standbys: Vec<Standby>,
    last_alive: Instant,
    window: Window,
    echoes: HashMap<u32, Instant>,
    flows: HashMap<SocketAddrV4, Flow>,
    carried: u64,
    reported_blocked: u64,
    /// When blocked game traffic was last logged.
    blocked_logged: Option<Instant>,
    renewal: Renewal,
    physical: HashMap<IpAddr, NextHop>,
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

impl Session {
    pub fn new(p: Params) -> Self {
        let now = Instant::now();
        let lifetime = Duration::from_secs(p.grant.expires_at.saturating_sub(p.grant.issued_at));
        let renewal = Renewal {
            lifetime,
            next_attempt: now + lifetime * 2 / 3,
            task: None,
            unacked: None,
            final_expiry: false,
            deadline: now + lifetime,
            pending_deadline: None,
        };
        let physical = p
            .host_routes
            .iter()
            .filter_map(|r| match *r {
                Route::Host { address, via } => Some((address, via)),
                Route::Adapter { .. } => None,
            })
            .collect();
        let standbys = p
            .standbys
            .iter()
            .map(|hop| Standby {
                hop: hop.clone(),
                last_refresh: None,
            })
            .collect();
        Self {
            p,
            standbys,
            last_alive: now,
            window: Window::default(),
            echoes: HashMap::new(),
            flows: HashMap::new(),
            carried: 0,
            reported_blocked: 0,
            blocked_logged: None,
            renewal,
            physical,
        }
    }

    pub async fn run(mut self, mut control: mpsc::Receiver<Control>) -> Ended {
        let mut echo_tick = tokio::time::interval(self.p.config.echo_interval);
        let mut second = tokio::time::interval(Duration::from_secs(1));
        self.p.guard.arm(self.p.game_routes.clone());
        loop {
            self.p.guard.beat();
            if self.p.guard.fired() {
                return self
                    .end(EndReason::FellBack(FallbackCause::EngineStalled))
                    .await;
            }
            let failure_at = self.last_alive + self.p.config.failure_after;
            let renew_at = self.renewal.next_attempt;
            tokio::select! {
                command = control.recv() => match command {
                    None | Some(Control::Stop) => return self.end(EndReason::Stopped).await,
                    Some(Control::Suspend) => {
                        return self.end(EndReason::FellBack(FallbackCause::Suspended)).await;
                    }
                    Some(Control::Freeze(d)) => std::thread::sleep(d),
                },
                packet = self.p.channels.incoming.recv() => match packet {
                    Some(packet) => self.outbound(packet).await,
                    None => return self.end(EndReason::FellBack(FallbackCause::AdapterLost)).await,
                },
                received = self.p.link.recv() => match received {
                    Ok((datagram, from)) => self.inbound(&datagram, from).await,
                    Err(e) => tracing::debug!(error = %e, "receive error"),
                },
                _ = sleep_until(failure_at) => {
                    if let Err(cause) = self.failover().await {
                        return self.end(EndReason::FellBack(cause)).await;
                    }
                }
                _ = echo_tick.tick() => self.echo().await,
                _ = second.tick() => {
                    if let Err(cause) = self.every_second().await {
                        return self.end(EndReason::FellBack(cause)).await;
                    }
                }
                _ = sleep_until(renew_at), if self.renewal.task.is_none() && !self.renewal.final_expiry => {
                    self.start_renewal();
                }
                result = async { self.renewal.task.as_mut().expect("guarded").await }, if self.renewal.task.is_some() => {
                    self.renewal.task = None;
                    if let Err(cause) = self.renewed(result).await {
                        return self.end(EndReason::FellBack(cause)).await;
                    }
                }
            }
        }
    }

    /// A packet from the adapter: into the tunnel if it is the game's.
    async fn outbound(&mut self, packet: Vec<u8>) {
        if let Err(stray) = self.p.filter.check(&packet) {
            self.p.strays.count(stray);
            return;
        }
        if let Ok(udp) = packet::parse_udp(&packet) {
            let now = Instant::now();
            self.flows
                .entry(udp.dst)
                .and_modify(|f| f.last_out = now)
                .or_insert(Flow {
                    last_out: now,
                    last_in: None,
                });
        }
        match self.p.core.encapsulate(&packet) {
            Ok(Some(frame)) => {
                self.carried += 1;
                self.send(self.p.current.address, &frame).await;
            }
            Ok(None) => {}
            Err(e) => tracing::debug!(error = %e, "packet not sent"),
        }
    }

    async fn send(&self, to: SocketAddr, frame: &[u8]) {
        if let Err(e) = self.p.link.send(to, frame).await {
            tracing::debug!(error = %e, "send failed");
        }
    }

    /// A datagram from a relay.
    async fn inbound(&mut self, datagram: &[u8], from: SocketAddr) {
        let handled = self.p.core.handle(datagram, Some(from.ip()));
        for frame in &handled.send {
            self.send(from, frame).await;
        }
        let current = from == self.p.current.address;
        if handled.authenticated && current {
            self.last_alive = Instant::now();
        }
        for event in handled.events {
            match event {
                Event::Packet(packet) => self.deliver(packet),
                Event::Echo(seq) => {
                    if let Some(sent) = self.echoes.remove(&seq) {
                        self.window
                            .answered(sent.into_std(), Instant::now().into_std());
                    }
                }
                Event::Verify(cookie) => {
                    self.p.link.set_cookie(from, cookie);
                    if current {
                        // Our address changed, or the cookie aged out during
                        // a HELLO: say it again, with the new cookie.
                        let hello = self
                            .p
                            .link
                            .hello(&self.p.core, &self.p.token, &self.p.current);
                        self.send(from, &hello).await;
                    }
                }
                Event::Accepted { needs_handshake } if current => {
                    if needs_handshake {
                        self.exit_restarted().await;
                    }
                    if self.renewal.unacked.take().is_some() {
                        if let Some(deadline) = self.renewal.pending_deadline.take() {
                            self.renewal.deadline = deadline;
                        }
                        tracing::info!(session = %self.p.grant.session_id, "renewed token accepted by the exit");
                        let _ = self
                            .p
                            .reports
                            .send(Report::Renewed {
                                expires_at: self.p.grant.expires_at,
                            })
                            .await;
                    }
                }
                Event::HandshakeCompleted if !current => {
                    // A late answer to a failover initiation: our confirming
                    // keepalive (sent above) moves the exit's return path to
                    // this hop, so we send there too, or its replies would
                    // look like silence and start another failover.
                    let hop = self
                        .standbys
                        .iter()
                        .find(|s| s.hop.address == from)
                        .map(|s| s.hop.clone());
                    if let Some(hop) = hop {
                        tracing::info!(relay = %hop.name, "a late handshake completed through a standby: following the exit's return path there");
                        let _ = self.switch_to(hop, Instant::now(), 0).await;
                    }
                }
                Event::CookieReply if current => {
                    if let Some(init) = self.p.core.handshake_initiation() {
                        self.send(from, &init).await;
                    }
                }
                Event::Refused(status) if current => {
                    tracing::warn!(?status, "the relay refused the session");
                    if status == HelloStatus::Expired {
                        self.renewal.next_attempt = Instant::now();
                    }
                }
                _ => {}
            }
        }
    }

    /// Writes a packet from the exit to the adapter, if it is the game's.
    fn deliver(&mut self, packet: Vec<u8>) {
        if let Err(stray) = self.p.filter.check_inbound(&packet) {
            self.p.strays.count(stray);
            return;
        }
        if let Ok(udp) = packet::parse_udp(&packet) {
            if let Some(flow) = self.flows.get_mut(&udp.src) {
                flow.last_in = Some(Instant::now());
            }
        }
        // A full queue drops the packet, as a real network would.
        let _ = self.p.channels.outgoing.try_send(packet);
    }

    async fn exit_restarted(&mut self) {
        tracing::warn!(session = %self.p.grant.session_id, "the exit had lost the session (it restarted): the game server now sees new ports");
        if let Some(init) = self.p.core.handshake_initiation() {
            self.send(self.p.current.address, &init).await;
        }
        let _ = self.p.reports.send(Report::AddressChanged).await;
    }

    async fn echo(&mut self) {
        for frame in self.p.core.tick() {
            self.send(self.p.current.address, &frame).await;
        }
        match self.p.core.echo() {
            Ok((seq, Some(frame))) => {
                let now = Instant::now();
                self.echoes.insert(seq, now);
                self.window.sent(now.into_std());
                self.send(self.p.current.address, &frame).await;
                // Echoes older than the window are never answered now.
                self.echoes
                    .retain(|_, sent| sent.elapsed() < crate::stats::WINDOW);
            }
            Ok((_, None)) => {}
            Err(e) => tracing::debug!(error = %e, "echo not sent"),
        }
    }

    /// Once a second: the graph, game-flow liveness, standby cookies, the
    /// network, and renewal deadlines.
    async fn every_second(&mut self) -> Result<(), FallbackCause> {
        let now = Instant::now();
        let figures = self
            .window
            .figures(now.into_std(), self.p.config.echo_interval * 2);
        let _ = self
            .p
            .reports
            .send(Report::Sample(Sample {
                at_unix_ms: unix_ms(),
                optimized: true,
                ping_ms: figures.rtt_ms.map(|rtt| rtt + self.p.exit_to_region_ms),
                jitter_ms: figures.jitter_ms,
                loss_pct: figures.loss_pct,
            }))
            .await;
        let _ = self.p.reports.send(Report::Carried(self.carried)).await;

        let blocked = self.p.strays.affecting_game();
        if blocked >= self.reported_blocked + 20 {
            self.reported_blocked = blocked;
            // The notice asks the player for a diagnostic report, which
            // only has what the log says (PLAN.md section 9).
            if self
                .blocked_logged
                .is_none_or(|at| at.elapsed() >= BLOCKED_LOG_EVERY)
            {
                self.blocked_logged = Some(Instant::now());
                let reasons: Vec<String> = self
                    .p
                    .strays
                    .summary()
                    .into_iter()
                    .filter(|(stray, _)| stray.affects_game())
                    .map(|(stray, n)| format!("{}={n}", stray.label()))
                    .collect();
                tracing::warn!(
                    packets = blocked,
                    reasons = %reasons.join(" "),
                    "game traffic the tunnel can't carry was dropped (IPv6, not UDP, or a port the game's definition doesn't list)"
                );
            }
            let _ = self
                .p
                .reports
                .send(Report::GameTrafficBlocked { packets: blocked })
                .await;
        }

        // A flow that had answers and gets none while the path answers:
        // the exit stopped delivering it, and another entry won't help.
        self.flows.retain(|_, f| f.last_out.elapsed() < FLOW_IDLE);
        let path_fresh = self.last_alive.elapsed() < self.p.config.failure_after;
        let stalled = self.flows.iter().find(|(_, f)| {
            f.last_in.is_some_and(|last_in| {
                f.last_out.saturating_duration_since(last_in) > FLOW_SILENCE
                    && f.last_out.elapsed() < Duration::from_secs(1)
            })
        });
        if let (true, Some((dst, _))) = (path_fresh, stalled) {
            tracing::warn!(game_server = %pingpath_diagnostics::Redacted(IpAddr::V4(*dst.ip())), "the exit answers but stopped delivering the game's traffic");
            return Err(FallbackCause::ExitNotForwarding);
        }

        self.refresh_standbys().await;
        self.check_network().await;

        // The token the exit holds, checked every second whatever renewal
        // is doing (in flight, failed and waiting to retry, unacknowledged,
        // or done for good), with a margin before the relays' cutoff
        // (expiry + 60 s skew).
        if self.renewal.deadline <= now {
            return Err(if self.renewal.final_expiry {
                FallbackCause::SessionEnded
            } else {
                FallbackCause::SessionExpired
            });
        }
        if let Some(sent) = self.renewal.unacked {
            if sent.elapsed() >= RENEW_HELLO_EVERY {
                let hello = self
                    .p
                    .link
                    .hello(&self.p.core, &self.p.token, &self.p.current);
                self.send(self.p.current.address, &hello).await;
                self.renewal.unacked = Some(Instant::now());
            }
        }
        Ok(())
    }

    /// Keeps a fresh cookie from each standby entry, so failover can send
    /// HELLO and the handshake at once.
    async fn refresh_standbys(&mut self) {
        let due: Vec<Hop> = self
            .standbys
            .iter_mut()
            .filter(|s| {
                s.hop.via_entry
                    && s.last_refresh
                        .is_none_or(|t| t.elapsed() >= STANDBY_REFRESH)
            })
            .map(|s| {
                s.last_refresh = Some(Instant::now());
                s.hop.clone()
            })
            .collect();
        for hop in due {
            // Without a cookie the entry answers with a fresh one and keeps
            // no state; the answer arrives in `inbound`.
            let hello = self.p.core.hello_with(&self.p.token, None);
            self.send(hop.address, &hello).await;
        }
    }

    /// Wi-Fi to cable, a new gateway: move the relay host routes to the
    /// new physical route, then HELLO and a handshake on the current path
    /// from our new address. Done before the watchdog spends its budget
    /// sending into a dead interface.
    async fn check_network(&mut self) {
        let mut changed = false;
        for (address, via) in self.physical.clone() {
            let Ok(now) = self.p.platform.best_route(address) else {
                continue;
            };
            if now == via {
                continue;
            }
            changed = true;
            let old = Route::Host { address, via };
            let new = Route::Host { address, via: now };
            // Each move writes the route ledger to disk (Windows), which a
            // busy disk can make slow. A beat between the steps shows the
            // guard the loop is making progress, so a few slow writes in a
            // row aren't taken for a hang that removes the game routes.
            self.p.guard.beat();
            if let Err(e) = self.p.platform.remove_route(&old) {
                tracing::debug!(error = %e, "could not remove the old host route");
            }
            self.p.guard.beat();
            match self.p.platform.add_route(&new) {
                Ok(()) => {
                    self.physical.insert(address, now);
                    if let Some(r) = self.p.host_routes.iter_mut().find(|r| **r == old) {
                        *r = new;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "could not move a relay host route"),
            }
        }
        if changed {
            tracing::info!("the network changed: moving the session to the new route");
            if let Some(via) = self.physical.get(&self.p.current.address.ip()) {
                self.p.link.prepare(&*self.p.platform, *via);
            }
            self.p.link.forget_cookies();
            let hello = self
                .p
                .link
                .hello(&self.p.core, &self.p.token, &self.p.current);
            self.send(self.p.current.address, &hello).await;
            if let Some(init) = self.p.core.handshake_initiation() {
                self.send(self.p.current.address, &init).await;
            }
        }
    }

    fn start_renewal(&mut self) {
        let Some(control) = self.p.control.clone() else {
            self.renewal.final_expiry = true;
            return;
        };
        let (grant, secret) = (self.p.grant.clone(), self.p.renew_secret.clone());
        self.renewal.task = Some(tokio::task::spawn_blocking(move || {
            control.renew(&grant, &secret)
        }));
    }

    /// A renewal came back.
    async fn renewed(
        &mut self,
        result: Result<Result<Grant, ControlError>, tokio::task::JoinError>,
    ) -> Result<(), FallbackCause> {
        let result = result.unwrap_or_else(|e| Err(ControlError::Network(e.to_string())));
        match result {
            Ok(grant) => {
                let token = grant.token_bytes().unwrap_or_default();
                if token.is_empty() {
                    self.renewal.next_attempt = Instant::now() + Duration::from_secs(30);
                    return Ok(());
                }
                let now = Instant::now();
                self.renewal.lifetime =
                    Duration::from_secs(grant.expires_at.saturating_sub(grant.issued_at));
                self.renewal.pending_deadline = Some(now + self.renewal.lifetime);
                self.renewal.next_attempt = now + self.renewal.lifetime * 2 / 3;
                self.renewal.final_expiry = grant.expires_at >= grant.renew_until;
                self.p.token = token;
                self.p.grant.token = grant.token;
                self.p.grant.expires_at = grant.expires_at;
                self.p.grant.issued_at = grant.issued_at;
                // Done only when the exit acknowledges the new token.
                let hello = self
                    .p
                    .link
                    .hello(&self.p.core, &self.p.token, &self.p.current);
                self.send(self.p.current.address, &hello).await;
                self.renewal.unacked = Some(Instant::now());
                Ok(())
            }
            Err(ControlError::Refused {
                code, retry_after, ..
            }) => match code.as_str() {
                // Final answers: plan the end now instead of retrying.
                "session_ended" => {
                    tracing::info!("the session reached its maximum lifetime");
                    self.renewal.final_expiry = true;
                    Ok(())
                }
                "unknown_session" | "game_not_routable" => {
                    tracing::warn!(%code, "renewal refused for good");
                    Err(FallbackCause::RenewalRefused)
                }
                _ => {
                    let wait = retry_after.map_or(Duration::from_secs(30), Duration::from_secs);
                    tracing::warn!(%code, wait_secs = wait.as_secs(), "renewal refused; trying again later");
                    self.renewal.next_attempt = Instant::now() + wait;
                    Ok(())
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "renewal failed; trying again in 30 s");
                self.renewal.next_attempt = Instant::now() + Duration::from_secs(30);
                Ok(())
            }
        }
    }

    /// The path failed: try the standbys (and the current hop, in case it
    /// was a blip) at once, through the same handshake initiation; the
    /// first answer wins. Falls back only if none answers in time and the
    /// direct route works.
    async fn failover(&mut self) -> Result<(), FallbackCause> {
        let started = Instant::now();
        let _ = self.p.reports.send(Report::FailingOver).await;
        tracing::warn!(
            silent_ms = self.last_alive.elapsed().as_millis() as u64,
            relay = %self.p.current.name,
            "path failed: no authenticated traffic from the exit"
        );
        let mut targets: Vec<Hop> = self
            .standbys
            .iter()
            .map(|s| s.hop.clone())
            .take(PARALLEL_STANDBYS)
            .collect();
        targets.push(self.p.current.clone());

        // Is the direct route alive? If not, the problem is local and
        // falling back would only change the server-visible address. The
        // check is bounded to finish before the deadline, so the decision
        // there never depends on how fast it happened to be.
        let direct = {
            let probe = self.p.direct.clone();
            let region = self.p.region.clone();
            let within = self.p.config.failover_budget.mul_f64(DIRECT_CHECK_SHARE);
            tokio::task::spawn_blocking(move || probe.reachable(&region, within))
        };
        let mut direct = Some(direct);
        let mut direct_alive: Option<bool> = None;

        let retransmit =
            Duration::from_secs_f64(self.p.path_rtt_ms * 1.25 / 1000.0).max(MIN_RETRANSMIT);
        let mut deadline = started + self.p.config.failover_budget;
        let mut extended = false;
        let mut attempts = 0u32;
        // An exit that restarted answers every hop's HELLO as a new
        // session; the player is told once.
        let mut told_new_session = false;
        loop {
            // Send HELLO (with each entry's cookie) and one initiation to all.
            attempts += 1;
            let init = self.p.core.handshake_initiation();
            for hop in &targets {
                let hello = self.p.link.hello(&self.p.core, &self.p.token, hop);
                self.send(hop.address, &hello).await;
                // The exit answers only the first copy of an initiation to
                // reach it, and the current hop's is usually the fastest. If
                // what failed is its way back, that answer is lost every
                // round, so from the second round it gets the initiation
                // only every other round, and a standby's copy can win. A
                // current hop that recovers is still noticed by any frame.
                let skip = attempts % 2 == 0 && targets.len() > 1 && *hop == self.p.current;
                if let (Some(init), false) = (&init, skip) {
                    self.send(hop.address, init).await;
                }
            }
            let next = (Instant::now() + retransmit).min(deadline);
            while Instant::now() < next {
                self.p.guard.beat();
                tokio::select! {
                    received = self.p.link.recv() => {
                        let Ok((datagram, from)) = received else { continue };
                        let Some(hop) = targets.iter().find(|h| h.address == from).cloned() else {
                            continue;
                        };
                        let handled = self.p.core.handle(&datagram, Some(from.ip()));
                        for frame in &handled.send {
                            self.send(from, frame).await;
                        }
                        let mut won = false;
                        for event in handled.events {
                            match event {
                                Event::HandshakeCompleted => won = true,
                                Event::Verify(cookie) => {
                                    // Its cookie had aged: answer at once.
                                    self.p.link.set_cookie(from, cookie);
                                    let hello = self.p.link.hello(&self.p.core, &self.p.token, &hop);
                                    self.send(from, &hello).await;
                                    if let Some(init) = &init {
                                        self.send(from, init).await;
                                    }
                                }
                                Event::Accepted { needs_handshake: true } if !told_new_session => {
                                    told_new_session = true;
                                    let _ = self.p.reports.send(Report::AddressChanged).await;
                                    tracing::warn!("the exit had lost the session during failover: the game server sees new ports");
                                }
                                Event::Packet(packet) => self.deliver(packet),
                                _ => {}
                            }
                        }
                        // The current path came back by itself, or another won.
                        if won || (handled.authenticated && from == self.p.current.address) {
                            return self.switch_to(hop, started, attempts).await;
                        }
                    }
                    // Packets that reach the adapter now would be stale.
                    Some(_) = self.p.channels.incoming.recv() => {}
                    result = async { direct.as_mut().expect("guarded").await }, if direct.is_some() => {
                        direct = None;
                        direct_alive = Some(result.unwrap_or(true));
                    }
                    _ = sleep_until(next) => {}
                }
            }
            if Instant::now() < deadline {
                continue;
            }
            if let Some(pending) = direct.take() {
                direct_alive = Some(pending.await.unwrap_or(true));
            }
            if direct_alive == Some(false) && !extended {
                // Nothing works, not even direct: a local outage (Wi-Fi
                // dropping for a moment). Falling back can't bring the game
                // back and would change its address; the relays may.
                extended = true;
                deadline += OUTAGE_EXTENSION;
                tracing::warn!(
                    "the direct route doesn't answer either: retrying the relays before falling back"
                );
                continue;
            }
            tracing::warn!(
                after_ms = started.elapsed().as_millis() as u64,
                attempts,
                tried = targets.len(),
                "no path answered: falling back to the direct route"
            );
            return Err(FallbackCause::RelaysUnreachable);
        }
    }

    async fn switch_to(
        &mut self,
        hop: Hop,
        started: Instant,
        attempts: u32,
    ) -> Result<(), FallbackCause> {
        let now = Instant::now();
        self.last_alive = now;
        // Flow silence counts from the moment the path came back: replies
        // couldn't arrive during the failover, and a flow that had answers
        // keeps that status.
        for flow in self.flows.values_mut() {
            if flow.last_in.is_some() {
                flow.last_in = Some(now);
            }
        }
        let after_ms = started.elapsed().as_millis() as u64;
        if hop == self.p.current {
            tracing::info!(after_ms, attempts, relay = %hop.name, "the path recovered");
            return Ok(());
        }
        tracing::info!(after_ms, attempts, from = %self.p.current.name, to = %hop.name, "failed over to a standby with the same exit");
        let old = std::mem::replace(&mut self.p.current, hop.clone());
        self.standbys.retain(|s| s.hop != hop);
        self.standbys.push(Standby {
            hop: old,
            last_refresh: None,
        });
        self.echoes.clear();
        let _ = self
            .p
            .reports
            .send(Report::PathChanged { hop, after_ms })
            .await;
        Ok(())
    }

    /// Ends the session: game routes first (traffic goes direct at once),
    /// then the host routes.
    async fn end(mut self, reason: EndReason) -> Ended {
        self.p.guard.disarm();
        let started = Instant::now();
        for route in &self.p.game_routes {
            if let Err(e) = self.p.platform.remove_route(route) {
                tracing::warn!(error = %e, "could not remove a game route");
            }
        }
        tracing::info!(reason = ?reason, removal_ms = started.elapsed().as_millis() as u64, "game routes removed");
        for route in &self.p.host_routes {
            if let Err(e) = self.p.platform.remove_route(route) {
                tracing::debug!(error = %e, "could not remove a host route");
            }
        }
        if let Some(task) = self.renewal.task.take() {
            task.abort();
        }
        Ended {
            channels: self.p.channels,
            reason,
        }
    }
}
