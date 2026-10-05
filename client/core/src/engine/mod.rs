//! The engine: measures a game's regions and the relay paths, turns
//! Optimize on only when it is measurably better, keeps the session up
//! (src/engine/session.rs) and falls back to the direct route when it can't
//! (docs/decisions/007-009).
//!
//! It runs as one task; [`EngineHandle`] sends it requests and receives its
//! status and live samples. The Windows service owns one engine.

pub mod guard;
pub mod link;
pub mod session;
pub mod types;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pingpath_games::{GameDefinition, RoutingPolicy};
use pingpath_tunnel::core::TunnelCore;
use pingpath_tunnel::keys::{self, WireGuardKeys};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::config::ClientConfig;
use crate::control::{ControlClient, ControlError, Latency, Relay};
use crate::filter::{PacketFilter, StrayCounts};
use crate::measure::{self, DirectProbe, ProbePlan};
use crate::net::{Adapter, AdapterConfig, PacketChannels, Platform, Route};
use crate::plan;
use crate::policy::{self, Refusal};
use crate::select::{self, Candidate, Inputs, Leg};
use guard::Guard;
use link::{Hop, Link};
use session::{Control, EndReason, Ended, Params, Report, Session};
pub use types::*;

/// A region is one the player normally uses when its direct ping is within
/// this many milliseconds ...
const USUAL_MARGIN_MS: f64 = 20.0;
/// ... or this share of the best region's (rule 3).
const USUAL_MARGIN_PCT: f64 = 20.0;
/// Turning Optimize on re-measures a measurement older than this.
const MEASUREMENT_TTL: Duration = Duration::from_secs(300);
/// Time allowed to bring the session up through each candidate.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Echo round trips measured before deciding to add routes.
const VERIFY_ECHOES: usize = 4;
const NOTICES_KEPT: usize = 20;
/// The tunnel's MTU, for the adapter.
const ADAPTER_MTU: u16 = pingpath_tunnel::frame::TUNNEL_MTU as u16;

/// What the engine needs from the outside.
pub struct EngineParts {
    pub config: ClientConfig,
    pub games: Vec<GameDefinition>,
    pub platform: Arc<dyn Platform>,
    pub direct: Arc<dyn DirectProbe>,
    pub probes: ProbePlan,
}

type Reply<T> = oneshot::Sender<Result<T, EngineError>>;

enum Command {
    Measure {
        game: String,
        region: String,
        reply: Reply<Measurement>,
    },
    Enable {
        acknowledged_warning: bool,
        reply: Reply<Status>,
    },
    Disable {
        reply: Reply<Status>,
    },
    Suspend,
    SetConfig {
        config: Box<ClientConfig>,
        reply: Reply<()>,
    },
    Dismiss {
        id: u64,
    },
    Freeze(Duration),
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// Talks to a running engine. Cheap to clone.
#[derive(Clone)]
pub struct EngineHandle {
    commands: mpsc::Sender<Command>,
    status: watch::Receiver<Status>,
    events: broadcast::Sender<EngineEvent>,
}

impl EngineHandle {
    async fn ask<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T, EngineError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(make(reply))
            .await
            .map_err(|_| EngineError::Stopped)?;
        answer.await.map_err(|_| EngineError::Stopped)?
    }

    /// Measures the direct route to every region of `game` and the relay
    /// paths to `region`.
    pub async fn measure(&self, game: &str, region: &str) -> Result<Measurement, EngineError> {
        let (game, region) = (game.to_string(), region.to_string());
        self.ask(|reply| Command::Measure {
            game,
            region,
            reply,
        })
        .await
    }

    /// Turns Optimize on for the measured game and region.
    pub async fn enable(&self, acknowledged_warning: bool) -> Result<Status, EngineError> {
        self.ask(|reply| Command::Enable {
            acknowledged_warning,
            reply,
        })
        .await
    }

    /// Turns Optimize off: game routes first.
    pub async fn disable(&self) -> Result<Status, EngineError> {
        self.ask(|reply| Command::Disable { reply }).await
    }

    /// The PC is going to sleep: fall back cleanly, and say so.
    pub async fn suspend(&self) {
        let _ = self.commands.send(Command::Suspend).await;
    }

    pub async fn set_config(&self, config: ClientConfig) -> Result<(), EngineError> {
        self.ask(|reply| Command::SetConfig {
            config: Box::new(config),
            reply,
        })
        .await
    }

    /// The player dismissed a notice.
    pub async fn dismiss(&self, id: u64) {
        let _ = self.commands.send(Command::Dismiss { id }).await;
    }

    pub fn status(&self) -> Status {
        self.status.borrow().clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    /// Turns Optimize off and stops the engine.
    pub async fn shutdown(&self) {
        let (reply, done) = oneshot::channel();
        if self
            .commands
            .send(Command::Shutdown { reply })
            .await
            .is_ok()
        {
            let _ = done.await;
        }
    }

    /// Test hook: blocks the session's loop, as a hang would.
    #[doc(hidden)]
    pub async fn freeze_session(&self, duration: Duration) {
        let _ = self.commands.send(Command::Freeze(duration)).await;
    }
}

/// Starts an engine on the current tokio runtime.
pub fn start(parts: EngineParts) -> EngineHandle {
    let (commands, rx) = mpsc::channel(32);
    let (status_tx, status) = watch::channel(Status::default());
    let (events, _) = broadcast::channel(256);
    let guard = Arc::new(Guard::start(
        parts.platform.clone(),
        parts.config.failure_after,
    ));
    let engine = Engine {
        control: ControlClient::new(&parts.config.control_url).ok(),
        parts,
        adapter: None,
        measured: None,
        running: None,
        status: Status::default(),
        status_tx,
        events: events.clone(),
        notices: VecDeque::new(),
        next_notice: 1,
        strays: Arc::new(StrayCounts::default()),
        guard,
        direct_sample: None,
        next_direct_sample: Instant::now(),
    };
    tokio::spawn(engine.run(rx));
    EngineHandle {
        commands,
        status,
        events,
    }
}

/// Everything a measurement found, kept for turning Optimize on.
struct Measured {
    game: GameDefinition,
    region: String,
    view: Measurement,
    candidates: Vec<Candidate>,
    relays: Vec<Relay>,
    at: Instant,
}

struct Running {
    control: mpsc::Sender<Control>,
    reports: mpsc::Receiver<Report>,
    task: JoinHandle<Ended>,
}

struct Engine {
    parts: EngineParts,
    control: Option<ControlClient>,
    adapter: Option<(Box<dyn Adapter>, Option<PacketChannels>)>,
    measured: Option<Measured>,
    running: Option<Running>,
    status: Status,
    status_tx: watch::Sender<Status>,
    events: broadcast::Sender<EngineEvent>,
    notices: VecDeque<Notice>,
    next_notice: u64,
    strays: Arc<StrayCounts>,
    guard: Arc<Guard>,
    direct_sample: Option<JoinHandle<Option<Leg>>>,
    next_direct_sample: Instant,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

impl Engine {
    async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        // Leftovers from a crash: a stale adapter and our routes.
        match self.parts.platform.cleanup() {
            Ok(report) if report != Default::default() => {
                tracing::warn!(
                    adapters = report.adapters,
                    routes = report.routes,
                    "removed leftovers from an earlier run"
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "cleanup of earlier leftovers failed"),
        }
        loop {
            let next_sample = self.next_direct_sample;
            tokio::select! {
                command = commands.recv() => match command {
                    Some(Command::Shutdown { reply }) => {
                        self.stop_session(None).await;
                        self.adapter = None;
                        tracing::info!("engine stopped");
                        let _ = reply.send(());
                        return;
                    }
                    Some(command) => self.handle(command).await,
                    None => {
                        self.stop_session(None).await;
                        return;
                    }
                },
                report = async { self.running.as_mut().expect("guarded").reports.recv().await }, if self.running.is_some() => {
                    match report {
                        Some(report) => self.report(report),
                        // The session dropped its sender: it ended by itself.
                        None => self.session_ended().await,
                    }
                }
                _ = tokio::time::sleep_until(next_sample), if self.sampling_direct() => {
                    self.sample_direct();
                }
                sample = async { self.direct_sample.as_mut().expect("guarded").await }, if self.direct_sample.is_some() => {
                    self.direct_sample = None;
                    let leg = sample.ok().flatten();
                    let _ = self.events.send(EngineEvent::Sample(Sample {
                        at_unix_ms: unix_ms(),
                        optimized: false,
                        ping_ms: leg.map(|l| l.rtt_ms),
                        jitter_ms: None,
                        loss_pct: leg.map_or(100.0, |l| l.loss_pct),
                    }));
                }
            }
        }
    }

    /// While a game is measured but not optimized, the graph shows the
    /// direct ping every few seconds.
    fn sampling_direct(&self) -> bool {
        self.status.mode == Mode::Ready && self.direct_sample.is_none() && self.measured.is_some()
    }

    fn sample_direct(&mut self) {
        self.next_direct_sample = Instant::now() + Duration::from_secs(2);
        let Some(m) = &self.measured else { return };
        let Some(region) = m.game.region(&m.region).cloned() else {
            return;
        };
        let direct = self.parts.direct.clone();
        self.direct_sample = Some(tokio::task::spawn_blocking(move || direct.measure(&region)));
    }

    async fn handle(&mut self, command: Command) {
        match command {
            Command::Measure {
                game,
                region,
                reply,
            } => {
                let result = self.measure(&game, &region).await;
                if let Err(EngineError::Refused { refusal }) = &result {
                    self.notice(NoticeKind::Refused {
                        refusal: refusal.clone(),
                    });
                }
                let _ = reply.send(result);
            }
            Command::Enable {
                acknowledged_warning,
                reply,
            } => {
                let result = self.enable(acknowledged_warning).await;
                match &result {
                    Err(EngineError::NotBetter {
                        direct_ms,
                        optimized_ms,
                    }) => {
                        self.notice(NoticeKind::NotBetter {
                            direct_ms: *direct_ms,
                            optimized_ms: *optimized_ms,
                        });
                    }
                    Err(EngineError::DirectUnknown) => self.notice(NoticeKind::DirectUnknown),
                    Err(
                        e @ (EngineError::Connect { .. }
                        | EngineError::Control { .. }
                        | EngineError::Platform { .. }),
                    ) => {
                        self.notice(NoticeKind::Error {
                            message: e.to_string(),
                        });
                    }
                    _ => {}
                }
                if result.is_err() && self.status.mode == Mode::Connecting {
                    self.set_mode(if self.measured.is_some() {
                        Mode::Ready
                    } else {
                        Mode::Idle
                    });
                }
                let _ = reply.send(result.map(|()| self.status.clone()));
            }
            Command::Disable { reply } => {
                self.stop_session(None).await;
                let _ = reply.send(Ok(self.status.clone()));
            }
            Command::Suspend => {
                if self.running.is_some() {
                    self.stop_session(Some(Control::Suspend)).await;
                }
            }
            Command::SetConfig { config, reply } => {
                let result = config
                    .validate()
                    .map_err(|message| EngineError::Platform { message });
                if result.is_ok() {
                    if config.control_url != self.parts.config.control_url {
                        self.control = ControlClient::new(&config.control_url).ok();
                    }
                    tracing::info!(
                        verbose_logging = config.verbose_logging,
                        min_improvement_ms = config.min_improvement_ms,
                        min_improvement_pct = config.min_improvement_pct,
                        "settings changed"
                    );
                    self.parts.config = *config;
                }
                let _ = reply.send(result);
            }
            Command::Dismiss { id } => {
                self.notices.retain(|n| n.id != id);
                self.publish();
            }
            Command::Freeze(d) => {
                if let Some(running) = &self.running {
                    let _ = running.control.send(Control::Freeze(d)).await;
                }
            }
            Command::Shutdown { .. } => unreachable!("handled in run"),
        }
    }

    fn game(&self, id: &str) -> Result<GameDefinition, EngineError> {
        self.parts
            .games
            .iter()
            .find(|g| g.id == id)
            .cloned()
            .ok_or_else(|| EngineError::Refused {
                refusal: Refusal::UnknownGame {
                    game: id.to_string(),
                },
            })
    }

    async fn measure(
        &mut self,
        game_id: &str,
        region_id: &str,
    ) -> Result<Measurement, EngineError> {
        if self.running.is_some() {
            return Err(EngineError::Busy {
                doing: "optimizing; turn Optimize off first".into(),
            });
        }
        let game = self.game(game_id)?;
        let policy =
            policy::check(&game, region_id).map_err(|refusal| EngineError::Refused { refusal })?;
        let overlaps = pingpath_games::overlaps_with_refused(&game, &self.parts.games);
        if !overlaps.is_empty() {
            tracing::warn!(game = %game.id, problems = ?overlaps, "refused: shares server ranges with a game that is never routed");
            return Err(EngineError::Refused {
                refusal: Refusal::SharesRangesWithRefusedGame {
                    game: game.name.clone(),
                },
            });
        }
        self.status.game = Some(game.id.clone());
        self.status.region = Some(region_id.to_string());
        self.status.measurement = None;
        self.measured = None;
        self.set_mode(Mode::Measuring);

        // Direct to every region (for rule 3), in parallel.
        let directs: Vec<_> = game
            .regions
            .iter()
            .map(|region| {
                let (probe, region) = (self.parts.direct.clone(), region.clone());
                tokio::task::spawn_blocking(move || probe.measure(&region))
            })
            .collect();
        let mut regions = Vec::new();
        for (region, task) in game.regions.iter().zip(directs) {
            regions.push(RegionMeasurement {
                region: region.id.clone(),
                name: region.name.clone(),
                direct: task.await.ok().flatten(),
                usual: false,
            });
        }
        let best_direct = regions
            .iter()
            .filter_map(|r| r.direct.map(|d| d.rtt_ms))
            .min_by(f64::total_cmp);
        for r in &mut regions {
            r.usual = match (r.direct, best_direct) {
                (Some(d), Some(best)) => {
                    d.rtt_ms <= best + USUAL_MARGIN_MS.max(best * USUAL_MARGIN_PCT / 100.0)
                }
                _ => false,
            };
        }
        let direct = regions
            .iter()
            .find(|r| r.region == region_id)
            .and_then(|r| r.direct);

        // The relays: from the control plane, and the player's own probes.
        let (relays, latency) = match &self.control {
            Some(control) => {
                let control = control.clone();
                let fetched = tokio::task::spawn_blocking(
                    move || -> Result<(Vec<Relay>, Latency), ControlError> {
                        Ok((control.relays()?, control.latency()?))
                    },
                )
                .await
                .unwrap_or_else(|e| Err(ControlError::Network(e.to_string())));
                match fetched {
                    Ok(fetched) => fetched,
                    Err(e) => {
                        self.set_mode(Mode::Idle);
                        return Err(EngineError::Control {
                            message: e.to_string(),
                        });
                    }
                }
            }
            None => {
                self.set_mode(Mode::Idle);
                return Err(EngineError::Control {
                    message: ControlError::NotConfigured.to_string(),
                });
            }
        };
        let probed: Vec<(u32, SocketAddr)> = relays
            .iter()
            .filter(|r| r.is_entry() || r.serves(region_id))
            .map(|r| (r.id, r.address))
            .collect();
        let player_to_relay = measure::probe_relays(&probed, self.parts.probes).await;
        let inputs = Inputs {
            relays: relays.clone(),
            player_to_relay,
            latency,
        };
        let candidates = select::candidates(&game.id, region_id, &inputs);
        let name = |id: u32| {
            relays
                .iter()
                .find(|r| r.id == id)
                .map_or_else(|| id.to_string(), |r| r.name.clone())
        };
        let best = candidates.first().map(|c| PathEstimate {
            exit: name(c.exit),
            entry: c.entry.map(name),
            estimate_ms: c.estimate_ms,
        });
        let worthwhile = match (direct, &best) {
            (Some(d), Some(b)) => select::worthwhile(
                d.rtt_ms,
                b.estimate_ms,
                self.parts.config.min_improvement_ms,
                self.parts.config.min_improvement_pct,
            ),
            _ => false,
        };
        tracing::info!(
            game = %game.id,
            region = %region_id,
            direct_ms = ?direct.map(|d| d.rtt_ms),
            best = ?best,
            candidates = candidates.len(),
            worthwhile,
            "measured"
        );
        let view = Measurement {
            game: game.id.clone(),
            region: region_id.to_string(),
            regions,
            direct,
            best,
            worthwhile,
            needs_warning: policy == RoutingPolicy::Warn,
            measured_at_unix: unix_now(),
        };
        self.measured = Some(Measured {
            game,
            region: region_id.to_string(),
            view: view.clone(),
            candidates,
            relays,
            at: Instant::now(),
        });
        self.status.measurement = Some(view.clone());
        self.next_direct_sample = Instant::now() + Duration::from_secs(2);
        self.set_mode(Mode::Ready);
        Ok(view)
    }

    async fn enable(&mut self, acknowledged_warning: bool) -> Result<(), EngineError> {
        if self.running.is_some() {
            return Ok(());
        }
        let Some(m) = &self.measured else {
            return Err(EngineError::NotMeasured);
        };
        if m.at.elapsed() > MEASUREMENT_TTL {
            let (game, region) = (m.game.id.clone(), m.region.clone());
            self.measure(&game, &region).await?;
        }
        let m = self.measured.as_ref().expect("measured above");
        let (game, region_id) = (m.game.clone(), m.region.clone());
        if m.view.needs_warning && !acknowledged_warning {
            return Err(EngineError::WarningNotAcknowledged);
        }
        let region_view = m.view.regions.iter().find(|r| r.region == region_id);
        let Some(direct) = m.view.direct else {
            return Err(EngineError::DirectUnknown);
        };
        if !region_view.is_some_and(|r| r.usual) {
            tracing::warn!(game = %game.id, region = %region_id, "refused: not a region the player normally uses (rule 3)");
            return Err(EngineError::NotUsualRegion { region: region_id });
        }
        let Some(best) = m.candidates.first().cloned() else {
            return Err(EngineError::NoPath);
        };
        if !m.view.worthwhile {
            return Err(EngineError::NotBetter {
                direct_ms: direct.rtt_ms,
                optimized_ms: best.estimate_ms,
            });
        }
        let relays = m.relays.clone();
        let mut hops: Vec<Hop> = Vec::new();
        for c in std::iter::once(&best).chain(select::standbys(&best, &m.candidates).iter()) {
            let id = c.entry.unwrap_or(c.exit);
            if let Some(r) = relays.iter().find(|r| r.id == id) {
                hops.push(Hop {
                    id,
                    name: r.name.clone(),
                    address: r.address,
                    via_entry: c.entry.is_some(),
                });
            }
        }
        // Rule 7, the routes and the filter, from the local definitions only.
        let relay_ips: Vec<_> = hops.iter().map(|h| h.address.ip()).collect();
        let route_plan = plan::plan(&game, &region_id, &relay_ips)
            .map_err(|refusal| EngineError::Refused { refusal })?;
        self.set_mode(Mode::Connecting);

        let control = self.control.clone().ok_or(EngineError::Control {
            message: ControlError::NotConfigured.to_string(),
        })?;
        let keys = WireGuardKeys::generate().map_err(|e| EngineError::Connect {
            message: e.to_string(),
        })?;
        let (client_key, game_id, region_for_grant, exit_id) = (
            keys.public_base64(),
            game.id.clone(),
            region_id.clone(),
            best.exit,
        );
        let grant = tokio::task::spawn_blocking(move || {
            control.create_session(&client_key, &game_id, &region_for_grant, Some(exit_id))
        })
        .await
        .unwrap_or_else(|e| Err(ControlError::Network(e.to_string())))
        .map_err(|e| EngineError::Control {
            message: e.to_string(),
        })?;
        let token = grant.token_bytes().map_err(|e| EngineError::Control {
            message: e.to_string(),
        })?;
        let exit_key = grant
            .exit
            .public_key
            .as_deref()
            .and_then(|k| keys::public_key_from_base64(k).ok())
            .ok_or(EngineError::Control {
                message: "the exit's key is missing".into(),
            })?;
        let claims = pingpath_tunnel::token::decode_unverified(&token).map_err(|e| {
            EngineError::Control {
                message: e.to_string(),
            }
        })?;
        let mut core = TunnelCore::new(
            claims.session_id,
            keys.secret.clone(),
            exit_key,
            self.parts.config.adapter_address,
        )
        .map_err(|e| EngineError::Connect {
            message: e.to_string(),
        })?;

        let mut channels = self.adapter_channels()?;
        while channels.incoming.try_recv().is_ok() {}
        let interface = self.adapter.as_ref().expect("created above").0.interface();
        let mut link = Link::bind(&hops).await.map_err(|e| EngineError::Connect {
            message: e.to_string(),
        })?;

        // Up through the best candidate, else the next with the same exit.
        let mut up = None;
        for (i, hop) in hops.iter().enumerate() {
            let deadline = Instant::now() + CONNECT_TIMEOUT;
            match link::connect(
                &mut core,
                &mut link,
                hop,
                &token,
                deadline,
                Some(&mut channels.incoming),
            )
            .await
            {
                Ok(connected) => {
                    up = Some(i);
                    for packet in connected.packets {
                        let _ = packet;
                    }
                    break;
                }
                Err(e) => {
                    tracing::warn!(relay = %hop.name, error = %e, "could not connect through this relay")
                }
            }
        }
        let Some(current) = up else {
            self.return_channels(channels);
            return Err(EngineError::Connect {
                message: "no relay answered".into(),
            });
        };
        let current_hop = hops[current].clone();
        let mut standbys = hops.clone();
        standbys.remove(current);

        // Measure the real path before routing anything.
        let rtt = match verify(&mut core, &mut link, &current_hop).await {
            Some(rtt) => rtt,
            None => {
                self.return_channels(channels);
                return Err(EngineError::Connect {
                    message: "the exit didn't answer through the tunnel".into(),
                });
            }
        };
        let optimized_ms = rtt + best.exit_to_region_ms;
        if !select::worthwhile(
            direct.rtt_ms,
            optimized_ms,
            self.parts.config.min_improvement_ms,
            self.parts.config.min_improvement_pct,
        ) {
            tracing::info!(
                direct_ms = direct.rtt_ms,
                optimized_ms,
                "not turned on: the direct route is as good"
            );
            self.return_channels(channels);
            return Err(EngineError::NotBetter {
                direct_ms: direct.rtt_ms,
                optimized_ms,
            });
        }

        // Routes: relay host routes first, then the game's.
        let mut host_routes = Vec::new();
        for ip in &route_plan.relay_hosts {
            match self.parts.platform.best_route(*ip) {
                Ok(via) => {
                    let route = Route::Host { address: *ip, via };
                    match self.parts.platform.add_route(&route) {
                        Ok(()) => host_routes.push(route),
                        Err(e) => {
                            tracing::warn!(relay = %pingpath_diagnostics::Redacted(*ip), error = %e, "could not add a relay host route")
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "no route to a relay"),
            }
        }
        let mut game_routes = Vec::new();
        for prefix in &route_plan.game_routes {
            let route = Route::Adapter {
                prefix: *prefix,
                interface,
            };
            match self.parts.platform.add_route(&route) {
                Ok(()) => {
                    tracing::info!(%prefix, "game route added");
                    game_routes.push(route);
                }
                Err(e) => {
                    tracing::error!(%prefix, error = %e, "could not add a game route; undoing");
                    for route in game_routes.iter().chain(host_routes.iter()) {
                        let _ = self.parts.platform.remove_route(route);
                    }
                    self.return_channels(channels);
                    return Err(EngineError::Platform {
                        message: e.to_string(),
                    });
                }
            }
        }

        let filter = PacketFilter::new(game.clone(), &region_id, self.parts.config.adapter_address);
        let (control_tx, control_rx) = mpsc::channel(8);
        let (reports_tx, reports) = mpsc::channel(64);
        let region = game.region(&region_id).cloned().expect("planned above");
        let session = Session::new(Params {
            core,
            link,
            current: current_hop.clone(),
            standbys,
            token,
            renew_secret: grant.renew_secret.clone().unwrap_or_default(),
            grant: grant.clone(),
            control: self.control.clone(),
            filter,
            channels,
            platform: self.parts.platform.clone(),
            game_routes,
            host_routes,
            exit_to_region_ms: best.exit_to_region_ms,
            path_rtt_ms: rtt,
            config: self.parts.config.clone(),
            strays: self.strays.clone(),
            reports: reports_tx,
            guard: self.guard.clone(),
            direct: self.parts.direct.clone(),
            region,
        });
        let task = tokio::spawn(session.run(control_rx));
        self.running = Some(Running {
            control: control_tx,
            reports,
            task,
        });
        tracing::info!(
            session = %grant.session_id,
            exit = %grant.exit.name,
            via = %current_hop.name,
            direct_ms = direct.rtt_ms,
            optimized_ms,
            "optimized"
        );
        self.status.path = Some(PathView {
            exit: grant.exit.name.clone(),
            entry: current_hop.via_entry.then(|| current_hop.name.clone()),
            since_unix: unix_now(),
            optimized_ms: Some(optimized_ms),
            direct_ms: Some(direct.rtt_ms),
        });
        self.status.carried = 0;
        self.set_mode(Mode::Optimized);
        Ok(())
    }

    /// The adapter's packet channels, creating the adapter on first use
    /// (creation is slow, and the first one installs the driver).
    fn adapter_channels(&mut self) -> Result<PacketChannels, EngineError> {
        if self.adapter.is_none() {
            let config = AdapterConfig {
                address: self.parts.config.adapter_address,
                mtu: ADAPTER_MTU,
            };
            let (adapter, channels) = self.parts.platform.create_adapter(&config).map_err(|e| {
                tracing::error!(error = %e, "cannot create the PingPath adapter");
                EngineError::Platform {
                    message: e.to_string(),
                }
            })?;
            tracing::info!(interface = adapter.interface().0, "adapter created");
            self.adapter = Some((adapter, Some(channels)));
        }
        self.adapter
            .as_mut()
            .and_then(|(_, channels)| channels.take())
            .ok_or(EngineError::Busy {
                doing: "the adapter is in use".into(),
            })
    }

    fn return_channels(&mut self, channels: PacketChannels) {
        if let Some((_, slot)) = &mut self.adapter {
            *slot = Some(channels);
        }
    }

    async fn stop_session(&mut self, why: Option<Control>) {
        let Some(running) = self.running.take() else {
            return;
        };
        let _ = running.control.send(why.unwrap_or(Control::Stop)).await;
        let ended = running.task.await.ok();
        self.finish(ended).await;
    }

    async fn session_ended(&mut self) {
        if let Some(running) = self.running.take() {
            let ended = running.task.await.ok();
            self.finish(ended).await;
        }
    }

    async fn finish(&mut self, ended: Option<Ended>) {
        self.guard.disarm();
        self.status.path = None;
        match ended {
            Some(Ended { channels, reason }) => {
                self.return_channels(channels);
                if let EndReason::FellBack(cause) = reason {
                    tracing::warn!(cause = ?cause, "fell back to the direct route");
                    self.notice(NoticeKind::FellBack { cause });
                }
            }
            None => {
                // The session task panicked; its routes went with it only if
                // the guard removed them. Drop the adapter to be sure.
                tracing::error!("the session stopped unexpectedly; removing the adapter");
                self.adapter = None;
                self.notice(NoticeKind::FellBack {
                    cause: FallbackCause::AdapterLost,
                });
            }
        }
        self.set_mode(if self.measured.is_some() {
            Mode::Ready
        } else {
            Mode::Idle
        });
    }

    fn report(&mut self, report: Report) {
        match report {
            Report::Sample(sample) => {
                let _ = self.events.send(EngineEvent::Sample(sample));
            }
            Report::FailingOver => self.set_mode(Mode::FailingOver),
            Report::PathChanged { hop, after_ms } => {
                if let Some(path) = &mut self.status.path {
                    path.entry = hop.via_entry.then(|| hop.name.clone());
                }
                tracing::info!(relay = %hop.name, after_ms, "path changed");
                self.notice(NoticeKind::FailedOver {
                    entry: hop.via_entry.then(|| hop.name.clone()),
                });
                self.set_mode(Mode::Optimized);
            }
            Report::AddressChanged => self.notice(NoticeKind::AddressChanged),
            Report::Renewed { expires_at } => tracing::debug!(expires_at, "session renewed"),
            Report::GameTrafficBlocked { packets } => {
                self.notices
                    .retain(|n| !matches!(n.kind, NoticeKind::GameTrafficBlocked { .. }));
                self.notice(NoticeKind::GameTrafficBlocked { packets });
            }
            Report::Carried(n) => {
                if self.status.carried != n {
                    self.status.carried = n;
                    self.status.blocked = self
                        .strays
                        .summary()
                        .into_iter()
                        .filter(|(s, _)| s.affects_game())
                        .map(|(s, n)| (s.label().to_string(), n))
                        .collect();
                    self.publish();
                }
                if self.status.mode == Mode::FailingOver {
                    // Recovered without a path change.
                    self.set_mode(Mode::Optimized);
                }
            }
        }
    }

    fn notice(&mut self, kind: NoticeKind) {
        let notice = Notice {
            id: self.next_notice,
            at_unix: unix_now(),
            kind,
        };
        self.next_notice += 1;
        self.notices.push_back(notice);
        while self.notices.len() > NOTICES_KEPT {
            self.notices.pop_front();
        }
        self.publish();
    }

    fn set_mode(&mut self, mode: Mode) {
        if self.status.mode != mode {
            tracing::debug!(?mode, "mode");
            self.status.mode = mode;
        }
        self.publish();
    }

    fn publish(&mut self) {
        self.status.notices = self.notices.iter().cloned().collect();
        let _ = self.status_tx.send(self.status.clone());
        let _ = self
            .events
            .send(EngineEvent::Status(Box::new(self.status.clone())));
    }
}

/// Measures the tunnel's real round trip with a few echoes; `None` if the
/// exit doesn't answer.
async fn verify(core: &mut TunnelCore, link: &mut Link, hop: &Hop) -> Option<f64> {
    use pingpath_tunnel::core::Event;
    let mut sent = std::collections::HashMap::new();
    let mut rtts = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(1);
    for _ in 0..VERIFY_ECHOES {
        if let Ok((seq, Some(frame))) = core.echo() {
            sent.insert(seq, Instant::now());
            let _ = link.send(hop.address, &frame).await;
        }
        let until = (Instant::now() + Duration::from_millis(100)).min(deadline);
        while let Ok(Ok((datagram, from))) = tokio::time::timeout_at(until, link.recv()).await {
            let handled = core.handle(&datagram, Some(from.ip()));
            for frame in &handled.send {
                let _ = link.send(from, frame).await;
            }
            for event in handled.events {
                if let Event::Echo(seq) = event {
                    if let Some(at) = sent.remove(&seq) {
                        rtts.push(at.elapsed().as_secs_f64() * 1000.0);
                    }
                }
            }
        }
    }
    while !sent.is_empty() && Instant::now() < deadline {
        let Ok(Ok((datagram, from))) = tokio::time::timeout_at(deadline, link.recv()).await else {
            break;
        };
        let handled = core.handle(&datagram, Some(from.ip()));
        for event in handled.events {
            if let Event::Echo(seq) = event {
                if let Some(at) = sent.remove(&seq) {
                    rtts.push(at.elapsed().as_secs_f64() * 1000.0);
                }
            }
        }
    }
    if rtts.is_empty() {
        return None;
    }
    rtts.sort_by(f64::total_cmp);
    Some(rtts[rtts.len() / 2])
}
