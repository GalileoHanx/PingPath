//! The engine against real relays and a real control plane on loopback,
//! with the in-memory network standing in for the player's PC. These are
//! PLAN.md Phase 3's done-when criteria for the client:
//!
//! - toggling Optimize never interrupts the UDP echo harness for more than 2 s;
//! - killing an entry moves traffic to another entry with the same exit,
//!   with no address change at the server;
//! - killing every relay falls back to the direct route within 2 s;
//! - only traffic to the game's ranges goes through the tunnel;
//! - untested and unsupported games are never routed.
//!
//! Every relay sits behind a proxy that adds delay and loss, so failover
//! is tested at round trips like Gulf to Frankfurt, not loopback's zero.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pingpath_client_core::config::ClientConfig;
use pingpath_client_core::engine::{
    self, EngineError, EngineHandle, EngineParts, FallbackCause, Mode, NoticeKind,
};
use pingpath_client_core::measure::{DirectProbe, Fixed, ProbePlan};
use pingpath_client_core::net::Route;
use pingpath_client_core::net::fake::{FakeGame, FakeNetwork, Path};
use pingpath_client_core::policy::Refusal;
use pingpath_client_core::select::Leg;
use pingpath_control::config::{
    Config as ControlConfig, RelayEntry, Role as ControlRole, SessionLimits,
};
use pingpath_control::state::{api_key_hash, hex};
use pingpath_control::{Control, ControlOptions};
use pingpath_games::{
    AntiCheat, GameDefinition, PingTarget, PortRange, Ports, Region, SupportStatus,
};
use pingpath_relay::config::{
    Config as RelayConfig, Control as ControlLink, Limits, Peer, ProbeConfig, Role,
};
use pingpath_relay::{Relay, RelayOptions};
use pingpath_tunnel::keys::{self, WireGuardKeys};
use pingpath_tunnel::token::Issuer;
use tokio::net::UdpSocket;

const LOCALHOST: Ipv4Addr = Ipv4Addr::LOCALHOST;
const REGION: &str = "here";
const EXIT: u32 = 1;
const ENTRY_A: u32 = 2;
const ENTRY_B: u32 = 3;
/// A second exit, in worlds that have one.
const EXIT_B: u32 = 4;
/// One way, between the player and the entries (near the player) ...
const ENTRY_DELAY: Duration = Duration::from_millis(50);
/// ... and the exit (near the game servers).
const EXIT_DELAY: Duration = Duration::from_millis(75);

/// A UDP server that echoes, and remembers who it saw.
struct Echo {
    addr: SocketAddrV4,
    seen: Arc<Mutex<Vec<SocketAddr>>>,
}

async fn echo_server() -> Echo {
    let socket = UdpSocket::bind((LOCALHOST, 0)).await.unwrap();
    let SocketAddr::V4(addr) = socket.local_addr().unwrap() else {
        unreachable!()
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            // Windows reports ICMP errors for earlier sends here.
            let Ok((n, from)) = socket.recv_from(&mut buf).await else {
                continue;
            };
            {
                let mut log = log.lock().unwrap();
                if log.last() != Some(&from) {
                    log.push(from);
                }
            }
            let _ = socket.send_to(&buf[..n], from).await;
        }
    });
    Echo { addr, seen }
}

/// Forwards UDP to `target` with `delay` each way and drops `loss` of it.
/// Stopping it (or the relay behind it) is how a test kills a path.
async fn delay_proxy(
    target: SocketAddr,
    delay: Duration,
    loss_per_mille: u64,
) -> (SocketAddr, Arc<AtomicBool>) {
    let front = Arc::new(UdpSocket::bind((LOCALHOST, 0)).await.unwrap());
    let back = Arc::new(UdpSocket::bind((LOCALHOST, 0)).await.unwrap());
    let addr = front.local_addr().unwrap();
    let alive = Arc::new(AtomicBool::new(true));
    let client: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
    let counter = Arc::new(AtomicU64::new(0));
    let drop_it = move |counter: &AtomicU64| {
        // Deterministic loss: every n-th datagram.
        loss_per_mille > 0 && counter.fetch_add(1, Ordering::Relaxed) % (1000 / loss_per_mille) == 7
    };
    {
        let (front, back, alive, client, counter) = (
            front.clone(),
            back.clone(),
            alive.clone(),
            client.clone(),
            counter.clone(),
        );
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                let Ok((n, from)) = front.recv_from(&mut buf).await else {
                    continue;
                };
                *client.lock().unwrap() = Some(from);
                if !alive.load(Ordering::Relaxed) || drop_it(&counter) {
                    continue;
                }
                let (back, data) = (back.clone(), buf[..n].to_vec());
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let _ = back.send_to(&data, target).await;
                });
            }
        });
    }
    {
        let alive = alive.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                let Ok((n, _)) = back.recv_from(&mut buf).await else {
                    continue;
                };
                let to = *client.lock().unwrap();
                if !alive.load(Ordering::Relaxed) || drop_it(&counter) {
                    continue;
                }
                if let Some(to) = to {
                    let (front, data) = (front.clone(), buf[..n].to_vec());
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = front.send_to(&data, to).await;
                    });
                }
            }
        });
    }
    (addr, alive)
}

fn game(
    id: &str,
    status: SupportStatus,
    echo: SocketAddrV4,
    ping_target: SocketAddr,
) -> GameDefinition {
    GameDefinition {
        id: id.into(),
        name: id.into(),
        status,
        status_reason: None,
        report: None,
        anti_cheat: vec![AntiCheat::None],
        ports: Ports {
            udp: vec![PortRange {
                first: echo.port(),
                last: echo.port(),
            }],
            tcp: vec![],
        },
        sdr_app_id: None,
        regions: vec![Region {
            id: REGION.into(),
            name: "This machine".into(),
            server_ranges: vec![format!("{}/32", echo.ip()).parse().unwrap()],
            ping_targets: vec![PingTarget::Tcp(ping_target)],
            sdr_pops: vec![],
            location: None,
        }],
        notes: None,
    }
}

fn relay_config(
    id: u32,
    roles: Vec<Role>,
    issuer: [u8; 32],
    listen: SocketAddr,
    relays: Vec<Peer>,
    control_url: &str,
) -> RelayConfig {
    RelayConfig {
        id,
        name: format!("relay-{id}"),
        listen,
        http: None,
        roles,
        private_key_file: None,
        regions: vec![REGION.into()],
        issuer_public_keys: vec![keys::encode(&issuer)],
        games_dir: None,
        limits: Limits::default(),
        relays,
        probe: ProbeConfig {
            interval_secs: 1,
            probes_per_round: 2,
        },
        control: Some(ControlLink {
            url: control_url.into(),
            api_key_file: "unused".into(),
        }),
    }
}

fn free_addr() -> SocketAddr {
    std::net::UdpSocket::bind((LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
}

struct World {
    relays: HashMap<u32, Relay>,
    proxies: HashMap<u32, Arc<AtomicBool>>,
    names: HashMap<String, u32>,
    control: Control,
    control_url: String,
    echo: Echo,
    games: Vec<GameDefinition>,
    _ping_listener: std::net::TcpListener,
}

impl World {
    /// Silences every relay's proxy (or brings them back), as a local
    /// outage would, without stopping the relays.
    fn silence(&self, quiet: bool) {
        for alive in self.proxies.values() {
            alive.store(!quiet, Ordering::Relaxed);
        }
    }

    /// Kills relay `id`: the relay stops and its proxy goes silent.
    fn kill(&mut self, id: u32) {
        self.proxies[&id].store(false, Ordering::Relaxed);
        if let Some(relay) = self.relays.remove(&id) {
            relay.shutdown();
        }
    }
}

/// An exit and two entries (each behind a delay proxy), a control plane
/// that lists the proxies, and the game's echo server.
async fn world(loss_per_mille: u64) -> World {
    build_world(loss_per_mille, &[EXIT]).await
}

/// The same with a second exit for the region; entries forward to both.
async fn world_with_two_exits() -> World {
    build_world(0, &[EXIT, EXIT_B]).await
}

async fn build_world(loss_per_mille: u64, exits: &[u32]) -> World {
    let issuer_seed = keys::random_32().unwrap();
    let issuer = Issuer::from_seed(&issuer_seed).unwrap().public_key();
    let exit_secrets: HashMap<u32, [u8; 32]> = exits
        .iter()
        .map(|&id| (id, keys::random_32().unwrap()))
        .collect();
    let all: Vec<u32> = exits.iter().copied().chain([ENTRY_A, ENTRY_B]).collect();
    let echo = echo_server().await;
    let ping_listener = std::net::TcpListener::bind((LOCALHOST, 0)).unwrap();
    let ping_target = ping_listener.local_addr().unwrap();
    let games = vec![
        game(
            "test-game",
            SupportStatus::Supported,
            echo.addr,
            ping_target,
        ),
        game(
            "testing-game",
            SupportStatus::Testing,
            echo.addr,
            ping_target,
        ),
        game(
            "untested-game",
            SupportStatus::Untested,
            SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 9), 9999),
            ping_target,
        ),
    ];
    let control_listen = std::net::TcpListener::bind((LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap();
    let control_url = format!("http://{control_listen}");
    let api_keys: HashMap<u32, String> = all
        .iter()
        .map(|&id| (id, hex(&keys::random_32().unwrap())))
        .collect();
    let addrs: HashMap<u32, SocketAddr> = all.iter().map(|&id| (id, free_addr())).collect();
    let peer = |id: u32, roles: Vec<Role>| Peer {
        id,
        name: format!("relay-{id}"),
        address: addrs[&id],
        roles,
    };

    let mut relays = HashMap::new();
    for &id in exits {
        let exit = Relay::start(RelayOptions {
            config: relay_config(
                id,
                vec![Role::Exit],
                issuer,
                addrs[&id],
                vec![
                    peer(ENTRY_A, vec![Role::Entry]),
                    peer(ENTRY_B, vec![Role::Entry]),
                ],
                &control_url,
            ),
            games: games.clone(),
            exit_secret: Some(exit_secrets[&id]),
            control_api_key: Some(api_keys[&id].clone()),
        })
        .await
        .unwrap();
        relays.insert(id, exit);
    }
    for id in [ENTRY_A, ENTRY_B] {
        let entry = Relay::start(RelayOptions {
            config: relay_config(
                id,
                vec![Role::Entry],
                issuer,
                addrs[&id],
                exits
                    .iter()
                    .map(|&exit| peer(exit, vec![Role::Exit]))
                    .collect(),
                &control_url,
            ),
            games: games.clone(),
            exit_secret: None,
            control_api_key: Some(api_keys[&id].clone()),
        })
        .await
        .unwrap();
        relays.insert(id, entry);
    }

    // The player reaches every relay through its proxy.
    let mut proxies = HashMap::new();
    let mut fronts = HashMap::new();
    for &id in &all {
        let delay = if exits.contains(&id) {
            EXIT_DELAY
        } else {
            ENTRY_DELAY
        };
        let (front, alive) = delay_proxy(addrs[&id], delay, loss_per_mille).await;
        proxies.insert(id, alive);
        fronts.insert(id, front);
    }
    let entry = |id: u32, roles: Vec<ControlRole>| RelayEntry {
        id,
        name: format!("relay-{id}"),
        address: fronts[&id],
        regions: if roles.contains(&ControlRole::Exit) {
            vec![REGION.into()]
        } else {
            vec![]
        },
        public_key: roles
            .contains(&ControlRole::Exit)
            .then(|| WireGuardKeys::from_secret(exit_secrets[&id]).public_base64()),
        api_key_sha256: Some(api_key_hash(&api_keys[&id])),
        roles,
        provider: None,
        location: None,
    };
    let control = Control::start(ControlOptions {
        config: ControlConfig {
            listen: control_listen,
            admin_listen: None,
            issuer_seed_file: "unused".into(),
            games_dir: None,
            client_ip_header: None,
            sessions: SessionLimits::default(),
            relays: exits
                .iter()
                .map(|&id| entry(id, vec![ControlRole::Exit]))
                .chain([
                    entry(ENTRY_A, vec![ControlRole::Entry]),
                    entry(ENTRY_B, vec![ControlRole::Entry]),
                ])
                .collect(),
        },
        games: games.clone(),
        issuer_seed,
    })
    .await
    .unwrap();

    // Wait until the relays have reported each other and the region.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let rows = control.state().latency(pingpath_tunnel::token::now_unix());
        let regions = control
            .state()
            .region_latency(pingpath_tunnel::token::now_unix());
        let measured = |entry, exit| {
            rows.iter().any(|r| {
                (r.relay_id == entry && r.peer_id == exit
                    || r.relay_id == exit && r.peer_id == entry)
                    && r.rtt_ms.is_some()
            })
        };
        if exits.iter().all(|&exit| {
            measured(ENTRY_A, exit)
                && measured(ENTRY_B, exit)
                && regions
                    .iter()
                    .any(|r| r.relay_id == exit && r.rtt_ms.is_some())
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the relays never reported their latency"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let names = all.iter().map(|&id| (format!("relay-{id}"), id)).collect();
    World {
        relays,
        proxies,
        names,
        control,
        control_url,
        echo,
        games,
        _ping_listener: ping_listener,
    }
}

fn start_engine(world: &World, net: &FakeNetwork, direct_ms: f64) -> EngineHandle {
    start_engine_with(
        world,
        net,
        Arc::new(Fixed(Some(Leg {
            rtt_ms: direct_ms,
            loss_pct: 0.0,
            samples: 5,
        }))),
    )
}

fn start_engine_with(
    world: &World,
    net: &FakeNetwork,
    direct: Arc<dyn DirectProbe>,
) -> EngineHandle {
    engine::start(EngineParts {
        config: ClientConfig {
            control_url: world.control_url.clone(),
            // The test relays are on loopback.
            allow_local_relays: true,
            ..ClientConfig::default()
        },
        games: world.games.clone(),
        platform: Arc::new(net.clone()),
        direct,
        probes: ProbePlan {
            probes: 3,
            spacing: Duration::from_millis(50),
            timeout: Duration::from_millis(600),
        },
    })
}

/// The UDP echo harness: a datagram every 20 ms, recording when each reply
/// came back and which way.
struct Harness {
    replies: Arc<Mutex<Vec<(Instant, Path)>>>,
    stop: Arc<AtomicBool>,
}

fn harness(game: FakeGame, server: SocketAddrV4) -> Harness {
    let replies = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let game = Arc::new(game);
    {
        let (game, stop) = (game.clone(), stop.clone());
        tokio::spawn(async move {
            let mut seq = 0u32;
            while !stop.load(Ordering::Relaxed) {
                seq += 1;
                let _ = game.send_to(server, &seq.to_be_bytes()).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
    }
    {
        let (replies, stop) = (replies.clone(), stop.clone());
        tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                if let Ok((_, _, path)) = game.recv(Duration::from_millis(200)).await {
                    replies.lock().unwrap().push((Instant::now(), path));
                }
            }
        });
    }
    Harness { replies, stop }
}

impl Harness {
    /// The longest time without a reply between `from` and `to`.
    fn longest_gap(&self, from: Instant, to: Instant) -> Duration {
        let replies = self.replies.lock().unwrap();
        let mut last = from;
        let mut longest = Duration::ZERO;
        for &(at, _) in replies.iter().filter(|(at, _)| *at >= from && *at <= to) {
            longest = longest.max(at - last);
            last = at;
        }
        longest.max(to.saturating_duration_since(last))
    }

    fn last_path(&self) -> Option<Path> {
        self.replies.lock().unwrap().last().map(|&(_, p)| p)
    }

    async fn wait_for(&self, path: Path, timeout: Duration) -> Instant {
        let deadline = Instant::now() + timeout;
        let start = Instant::now();
        loop {
            if let Some(&(at, _)) = self
                .replies
                .lock()
                .unwrap()
                .iter()
                .find(|(at, p)| *at >= start && *p == path)
            {
                return at;
            }
            assert!(
                Instant::now() < deadline,
                "no reply over {path:?} within {timeout:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

async fn optimize(engine: &EngineHandle) {
    let measured = engine.measure("test-game", REGION).await.unwrap();
    assert!(measured.worthwhile, "{measured:?}");
    engine.enable(false).await.unwrap();
    assert_eq!(engine.status().mode, Mode::Optimized);
}

fn adapter_routes(net: &FakeNetwork) -> Vec<Route> {
    net.routes()
        .into_iter()
        .filter(|r| matches!(r, Route::Adapter { .. }))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_games_traffic_goes_through_the_tunnel_and_toggling_keeps_the_game_connected() {
    let world = world(0).await;
    let net = FakeNetwork::new();
    let engine = start_engine(&world, &net, 400.0);
    let harness = harness(net.game().await.unwrap(), world.echo.addr);
    harness.wait_for(Path::Direct, Duration::from_secs(2)).await;

    let start = Instant::now();
    for _ in 0..3 {
        optimize(&engine).await;
        // Only the game's range goes to the adapter.
        let routes = adapter_routes(&net);
        assert_eq!(routes.len(), 1, "{routes:?}");
        assert_eq!(
            routes[0].prefix(),
            format!("{}/32", world.echo.addr.ip()).parse().unwrap()
        );
        assert_eq!(net.path_to(IpAddr::V4(*world.echo.addr.ip())), Path::Tunnel);
        for other in ["127.0.0.2", "198.51.100.1", "8.8.8.8"] {
            assert_eq!(net.path_to(other.parse().unwrap()), Path::Direct, "{other}");
        }
        harness.wait_for(Path::Tunnel, Duration::from_secs(2)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;

        engine.disable().await.unwrap();
        assert!(adapter_routes(&net).is_empty());
        harness.wait_for(Path::Direct, Duration::from_secs(2)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let gap = harness.longest_gap(start, Instant::now());
    assert!(
        gap < Duration::from_secs(2),
        "the harness went {gap:?} without a reply"
    );
    assert!(
        net.tunneled().iter().all(|dst| *dst == world.echo.addr),
        "only the game server went through the tunnel"
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killing_the_entry_moves_traffic_to_another_with_the_same_exit_and_server_address() {
    // A little loss on every path as well.
    let mut world = world(10).await;
    let net = FakeNetwork::new();
    let engine = start_engine(&world, &net, 400.0);
    let harness = harness(net.game().await.unwrap(), world.echo.addr);
    optimize(&engine).await;
    harness.wait_for(Path::Tunnel, Duration::from_secs(2)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let seen_before = world.echo.seen.lock().unwrap().last().copied().unwrap();

    let path = engine.status().path.unwrap();
    let entry = path.entry.clone().expect("through an entry");
    let killed = Instant::now();
    world.kill(world.names[&entry]);
    tokio::time::sleep(Duration::from_secs(3)).await;

    let status = engine.status();
    assert_eq!(status.mode, Mode::Optimized, "{status:?}");
    let now_via = status.path.as_ref().unwrap().entry.clone();
    assert_ne!(
        now_via.as_deref(),
        Some(entry.as_str()),
        "moved off the dead entry"
    );
    assert!(
        status
            .notices
            .iter()
            .any(|n| matches!(n.kind, NoticeKind::FailedOver { .. })),
        "{:?}",
        status.notices
    );
    assert!(
        !status
            .notices
            .iter()
            .any(|n| matches!(n.kind, NoticeKind::FellBack { .. })),
        "{:?}",
        status.notices
    );
    assert_eq!(harness.last_path(), Some(Path::Tunnel));
    let gap = harness.longest_gap(killed - Duration::from_millis(100), Instant::now());
    assert!(
        gap < Duration::from_secs(2),
        "the harness went {gap:?} without a reply"
    );
    // The game server still sees the same address: same exit, same flow.
    assert_eq!(
        world.echo.seen.lock().unwrap().last().copied().unwrap(),
        seen_before
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killing_every_relay_falls_back_to_direct_within_two_seconds() {
    let mut world = world(0).await;
    let net = FakeNetwork::new();
    let engine = start_engine(&world, &net, 400.0);
    let harness = harness(net.game().await.unwrap(), world.echo.addr);
    optimize(&engine).await;
    harness.wait_for(Path::Tunnel, Duration::from_secs(2)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let killed = Instant::now();
    for id in [EXIT, ENTRY_A, ENTRY_B] {
        world.kill(id);
    }
    let direct = harness.wait_for(Path::Direct, Duration::from_secs(4)).await;
    let last_tunnel = harness
        .replies
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|(at, p)| *p == Path::Tunnel && *at <= direct)
        .map_or(killed, |&(at, _)| at);
    let gap = direct - last_tunnel;
    assert!(
        gap < Duration::from_secs(2),
        "{gap:?} from the last reply through the tunnel to the first direct one"
    );
    assert!(adapter_routes(&net).is_empty());
    let status = engine.status();
    assert!(
        status.notices.iter().any(|n| n.kind
            == NoticeKind::FellBack {
                cause: FallbackCause::RelaysUnreachable
            }),
        "{:?}",
        status.notices
    );
    assert_eq!(status.mode, Mode::Ready);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn games_that_must_not_be_routed_never_are() {
    let world = world(0).await;
    let net = FakeNetwork::new();
    let engine = start_engine(&world, &net, 400.0);
    let refused = engine.measure("untested-game", REGION).await.unwrap_err();
    assert!(
        matches!(
            refused,
            EngineError::Refused {
                refusal: Refusal::NotRoutable { .. }
            }
        ),
        "{refused:?}"
    );
    assert!(matches!(
        engine.enable(true).await.unwrap_err(),
        EngineError::NotMeasured
    ));
    assert!(net.history().is_empty(), "no route was ever added");
    assert!(!net.has_adapter());

    // A game still being tested needs the warning acknowledged.
    let measured = engine.measure("testing-game", REGION).await.unwrap();
    assert!(measured.needs_warning);
    assert!(matches!(
        engine.enable(false).await.unwrap_err(),
        EngineError::WarningNotAcknowledged
    ));
    assert!(adapter_routes(&net).is_empty());
    engine.enable(true).await.unwrap();
    assert_eq!(adapter_routes(&net).len(), 1);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn not_turned_on_when_the_direct_route_is_as_good() {
    let world = world(0).await;
    let net = FakeNetwork::new();
    // Direct is faster than any path through the relays (50 ms each way).
    let engine = start_engine(&world, &net, 50.0);
    let measured = engine.measure("test-game", REGION).await.unwrap();
    assert!(!measured.worthwhile);
    assert!(matches!(
        engine.enable(false).await.unwrap_err(),
        EngineError::NotBetter { .. }
    ));
    assert!(adapter_routes(&net).is_empty());
    assert!(
        engine
            .status()
            .notices
            .iter()
            .any(|n| matches!(n.kind, NoticeKind::NotBetter { .. }))
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_guard_removes_the_routes_when_the_engine_hangs() {
    let world = world(0).await;
    let net = FakeNetwork::new();
    let engine = start_engine(&world, &net, 400.0);
    let harness = harness(net.game().await.unwrap(), world.echo.addr);
    optimize(&engine).await;
    harness.wait_for(Path::Tunnel, Duration::from_secs(2)).await;

    let frozen = Instant::now();
    engine.freeze_session(Duration::from_secs(4)).await;
    let direct = harness.wait_for(Path::Direct, Duration::from_secs(3)).await;
    assert!(
        direct - frozen < Duration::from_secs(2),
        "{:?} until traffic went direct",
        direct - frozen
    );
    assert!(adapter_routes(&net).is_empty());
    engine.shutdown().await;
}

/// The direct route as the engine sees it, switched off mid-test.
struct Switchable {
    leg: Leg,
    up: Arc<AtomicBool>,
}

impl DirectProbe for Switchable {
    fn measure(&self, _region: &Region) -> Option<Leg> {
        self.up.load(Ordering::Relaxed).then_some(self.leg)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_short_local_outage_keeps_the_path() {
    // Wi-Fi drops for a moment: the relays and the direct route go quiet
    // together. Falling back couldn't bring the game back and would change
    // its address, so the session gives its relays longer.
    let world = world(0).await;
    let net = FakeNetwork::new();
    let up = Arc::new(AtomicBool::new(true));
    let direct = Switchable {
        leg: Leg {
            rtt_ms: 400.0,
            loss_pct: 0.0,
            samples: 5,
        },
        up: up.clone(),
    };
    let engine = start_engine_with(&world, &net, Arc::new(direct));
    let harness = harness(net.game().await.unwrap(), world.echo.addr);
    optimize(&engine).await;
    harness.wait_for(Path::Tunnel, Duration::from_secs(2)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let seen_before = world.echo.seen.lock().unwrap().last().copied().unwrap();

    let outage = Instant::now();
    up.store(false, Ordering::Relaxed);
    world.silence(true);
    // Past the 1.7 s at which a dead path with a working direct route
    // falls back.
    tokio::time::sleep(Duration::from_millis(2200)).await;
    world.silence(false);
    up.store(true, Ordering::Relaxed);
    harness.wait_for(Path::Tunnel, Duration::from_secs(3)).await;

    let went_direct = harness
        .replies
        .lock()
        .unwrap()
        .iter()
        .any(|&(at, p)| at >= outage && p == Path::Direct);
    assert!(!went_direct, "fell back during a local outage");
    let status = engine.status();
    assert_eq!(status.mode, Mode::Optimized, "{status:?}");
    assert!(
        !status
            .notices
            .iter()
            .any(|n| matches!(n.kind, NoticeKind::FellBack { .. })),
        "{:?}",
        status.notices
    );
    assert_eq!(
        world.echo.seen.lock().unwrap().last().copied().unwrap(),
        seen_before,
        "the server-visible address must not change"
    );
    engine.shutdown().await;
}

/// Waits until the control plane holds `expected` sessions: closing is
/// best effort, in the background.
async fn sessions_at_the_control_plane(world: &World, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while world.control.state().active_sessions() != expected {
        assert!(
            Instant::now() < deadline,
            "{} sessions at the control plane, expected {expected}",
            world.control.state().active_sessions()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turning_it_off_closes_the_session() {
    // Each session counts against the player's network (a carrier NAT is
    // shared by many players) until it is closed or its token runs out.
    // (A failed attempt's session is closed too: see the dead exit test.)
    let world = world(0).await;
    let net = FakeNetwork::new();
    let engine = start_engine(&world, &net, 400.0);
    optimize(&engine).await;
    sessions_at_the_control_plane(&world, 1).await;
    engine.disable().await.unwrap();
    sessions_at_the_control_plane(&world, 0).await;

    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_exit_is_passed_over_for_the_next() {
    // The best exit dies between measuring and turning Optimize on: every
    // path through it fails, so the next exit is tried.
    let mut world = world_with_two_exits().await;
    let net = FakeNetwork::new();
    let engine = start_engine(&world, &net, 400.0);
    let measured = engine.measure("test-game", REGION).await.unwrap();
    let best = measured.best.expect("a path").exit;
    let (dead, other) = if best == format!("relay-{EXIT}") {
        (EXIT, EXIT_B)
    } else {
        (EXIT_B, EXIT)
    };
    world.kill(dead);
    engine.enable(false).await.unwrap();
    let status = engine.status();
    assert_eq!(status.mode, Mode::Optimized);
    assert_eq!(status.path.unwrap().exit, format!("relay-{other}"));
    // The failed attempt's session was closed; the live one remains.
    sessions_at_the_control_plane(&world, 1).await;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_adapter_removes_its_routes() {
    // A crash takes the adapter, and with it every route through it.
    let world = world(0).await;
    let net = FakeNetwork::new();
    let engine = start_engine(&world, &net, 400.0);
    optimize(&engine).await;
    assert_eq!(adapter_routes(&net).len(), 1);
    engine.shutdown().await;
    assert!(adapter_routes(&net).is_empty());
    assert!(!net.has_adapter());
}
