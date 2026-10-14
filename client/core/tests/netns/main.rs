//! The engine with the Linux backend (a real TUN device and kernel routes)
//! against real relays, a real control plane and a UDP echo server, each in
//! its own network namespace from tools/netsim/client-netsim.sh. PLAN.md
//! Phase 3's done-when criteria, on a real kernel:
//!
//!   sudo tools/netsim/client-netsim.sh up --no-direct-exit
//!   sudo ip netns exec ppgame <test binary> --ignored --test-threads=1
//!
//! (--no-direct-exit makes the client go through an entry, so killing that
//! entry tests failover to the other one with the same exit.) Phase 4's
//! done-when, Stability mode against random loss on one entry, is the
//! second test; it applies the loss itself (`client-netsim.sh loss`).
//!
//! The test process starts in ppgame (the player's PC). Servers and relays
//! run on threads that join their namespace with setns (`InNetns`). Which
//! way a reply went is read from the address the echo server saw: the exit's
//! egress address (10.3.0.1) through the tunnel, the player's (10.1.0.2)
//! directly.

#![cfg(target_os = "linux")]

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod support;

use pingpath_client_core::config::{ClientConfig, StabilityMode};
use pingpath_client_core::engine::{
    self, EngineError, EngineHandle, EngineParts, FallbackCause, Mode, NoticeKind,
};
use pingpath_client_core::measure::{Fixed, ProbePlan};
use pingpath_client_core::net::Platform;
use pingpath_client_core::net::linux::{LinuxPlatform, ip};
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
use pingpath_tunnel::token::{Issuer, now_unix};
use support::{InNetns, ip_in};
use tokio::net::UdpSocket;

// Addresses from tools/netsim/client-netsim.sh.
const PLAYER: Ipv4Addr = Ipv4Addr::new(10, 1, 0, 2);
const EXIT_EGRESS: Ipv4Addr = Ipv4Addr::new(10, 3, 0, 1);
const CONTROL: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 2, 1, 30)), 8080);
const GAME_SERVER: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(10, 3, 1, 10), 7000);
const PING_TARGET: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 3, 1, 10)), 7001);
const OTHER_SERVER: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(10, 3, 2, 10), 7000);
const REGION: &str = "netsim";
const ADAPTER: &str = "pptun0";
const EXIT: u32 = 1;
const ENTRY_A: u32 = 2;
const ENTRY_B: u32 = 3;

fn relay_addr(id: u32) -> SocketAddr {
    let last = match id {
        EXIT => 21,
        ENTRY_A => 11,
        ENTRY_B => 12,
        _ => unreachable!(),
    };
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 2, 1, last)), 51820)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    Tunnel,
    Direct,
}

/// UDP echo that appends the IPv4 address it saw, and logs each change.
async fn echo(addr: SocketAddrV4, seen: Arc<Mutex<Vec<SocketAddr>>>) {
    let socket = UdpSocket::bind(addr).await.expect("bind echo");
    let mut buf = [0u8; 2048];
    while let Ok((n, from)) = socket.recv_from(&mut buf).await {
        {
            let mut seen = seen.lock().unwrap();
            if seen.last() != Some(&from) {
                seen.push(from);
            }
        }
        let SocketAddr::V4(v4) = from else { continue };
        let mut reply = buf[..n].to_vec();
        reply.extend_from_slice(&v4.ip().octets());
        let _ = socket.send_to(&reply, from).await;
    }
}

fn game() -> GameDefinition {
    game_with("test-game", SupportStatus::Supported, "10.3.1.0/24")
}

fn game_with(id: &str, status: SupportStatus, range: &str) -> GameDefinition {
    GameDefinition {
        id: id.into(),
        name: id.into(),
        status,
        status_reason: None,
        report: None,
        anti_cheat: vec![AntiCheat::None],
        ports: Ports {
            udp: vec![PortRange {
                first: GAME_SERVER.port(),
                last: GAME_SERVER.port(),
            }],
            tcp: vec![],
        },
        sdr_app_id: None,
        regions: vec![Region {
            id: REGION.into(),
            name: "ppserver".into(),
            server_ranges: vec![range.parse().unwrap()],
            ping_targets: vec![PingTarget::Tcp(PING_TARGET)],
            sdr_pops: vec![],
            location: None,
        }],
        notes: None,
    }
}

fn relay_config(id: u32, roles: Vec<Role>, issuer: [u8; 32], relays: Vec<Peer>) -> RelayConfig {
    RelayConfig {
        id,
        name: format!("relay-{id}"),
        listen: relay_addr(id),
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
            url: format!("http://{CONTROL}"),
            api_key_file: "unused".into(),
        }),
        doctor: None,
    }
}

/// Re-adds relay addresses deleted by the test, even if it panics, so the
/// namespaces stay usable for the next run.
struct Restore(Vec<SocketAddr>);

impl Drop for Restore {
    fn drop(&mut self) {
        for addr in &self.0 {
            let _ = ip_in(
                "pprelays",
                &["addr", "replace", &format!("{}/32", addr.ip()), "dev", "lo"],
            );
        }
    }
}

/// Kills a relay the way a network failure does: its address vanishes
/// behind pprelays' blackhole route (no ICMP), the process keeps running.
fn kill(id: u32) {
    ip_in(
        "pprelays",
        &[
            "addr",
            "del",
            &format!("{}/32", relay_addr(id).ip()),
            "dev",
            "lo",
        ],
    )
    .expect("delete relay address");
}

struct World {
    _servers: InNetns,
    _control: InNetns,
    _relays: Vec<InNetns>,
    seen: Arc<Mutex<Vec<SocketAddr>>>,
    games: Vec<GameDefinition>,
}

async fn world() -> World {
    let issuer_seed = keys::random_32().unwrap();
    let issuer = Issuer::from_seed(&issuer_seed).unwrap().public_key();
    let exit_secret = keys::random_32().unwrap();
    // The other server's range belongs to a game that is never routed.
    let games = vec![
        game(),
        game_with("untested-game", SupportStatus::Untested, "10.3.2.0/24"),
    ];

    let seen = Arc::new(Mutex::new(Vec::new()));
    let servers = {
        let seen = seen.clone();
        InNetns::spawn("ppserver", move || async move {
            // The exits' region probe connects here (TCP connect time); the
            // kernel completes the handshake without accept().
            let _ping = std::net::TcpListener::bind(PING_TARGET).expect("bind ping target");
            tokio::join!(echo(GAME_SERVER, seen), echo(OTHER_SERVER, Arc::default()));
        })
        .unwrap()
    };

    let api_keys: HashMap<u32, String> = [EXIT, ENTRY_A, ENTRY_B]
        .iter()
        .map(|&id| (id, hex(&keys::random_32().unwrap())))
        .collect();
    let entry = |id: u32, roles: Vec<ControlRole>| RelayEntry {
        id,
        name: format!("relay-{id}"),
        address: relay_addr(id),
        regions: if roles.contains(&ControlRole::Exit) {
            vec![REGION.into()]
        } else {
            vec![]
        },
        public_key: roles
            .contains(&ControlRole::Exit)
            .then(|| WireGuardKeys::from_secret(exit_secret).public_base64()),
        api_key_sha256: Some(api_key_hash(&api_keys[&id])),
        roles,
        provider: None,
        location: None,
        doctor: None,
    };
    let control_config = ControlConfig {
        listen: CONTROL,
        admin_listen: None,
        issuer_seed_file: "unused".into(),
        games_dir: None,
        client_ip_header: None,
        sessions: SessionLimits::default(),
        doctor: Default::default(),
        relays: vec![
            entry(EXIT, vec![ControlRole::Exit]),
            entry(ENTRY_A, vec![ControlRole::Entry]),
            entry(ENTRY_B, vec![ControlRole::Entry]),
        ],
    };
    // The control plane reports "measured" once the relays have reported
    // entry <-> exit and exit -> region latency.
    let (measured_tx, measured_rx) = std::sync::mpsc::channel::<()>();
    let control = {
        let games = games.clone();
        InNetns::spawn("pprelays", move || async move {
            let control = Control::start(ControlOptions {
                config: control_config,
                games,
                issuer_seed,
            })
            .await
            .expect("control plane starts");
            loop {
                let rows = control.state().latency(now_unix());
                let regions = control.state().region_latency(now_unix());
                let measured = |e: u32| {
                    rows.iter().any(|r| {
                        (r.relay_id == e && r.peer_id == EXIT
                            || r.relay_id == EXIT && r.peer_id == e)
                            && r.rtt_ms.is_some()
                    })
                };
                if measured(ENTRY_A)
                    && measured(ENTRY_B)
                    && regions
                        .iter()
                        .any(|r| r.relay_id == EXIT && r.rtt_ms.is_some())
                {
                    let _ = measured_tx.send(());
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            std::future::pending::<()>().await; // keep `control` alive
        })
        .unwrap()
    };

    let peer = |id: u32, roles: Vec<Role>| Peer {
        id,
        name: format!("relay-{id}"),
        address: relay_addr(id),
        roles,
    };
    let mut relays = Vec::new();
    for (id, roles, peers, secret) in [
        (
            EXIT,
            vec![Role::Exit],
            vec![
                peer(ENTRY_A, vec![Role::Entry]),
                peer(ENTRY_B, vec![Role::Entry]),
            ],
            Some(exit_secret),
        ),
        (
            ENTRY_A,
            vec![Role::Entry],
            vec![peer(EXIT, vec![Role::Exit])],
            None,
        ),
        (
            ENTRY_B,
            vec![Role::Entry],
            vec![peer(EXIT, vec![Role::Exit])],
            None,
        ),
    ] {
        let (config, games, key) = (
            relay_config(id, roles, issuer, peers),
            games.clone(),
            api_keys[&id].clone(),
        );
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        relays.push(
            InNetns::spawn("pprelays", move || async move {
                let relay = Relay::start(RelayOptions {
                    config,
                    games,
                    exit_secret: secret,
                    control_api_key: Some(key),
                })
                .await
                .expect("relay starts");
                let _ = started_tx.send(());
                std::future::pending::<()>().await;
                drop(relay);
            })
            .unwrap(),
        );
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("relay started");
    }
    measured_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the relays never reported their latency");
    World {
        _servers: servers,
        _control: control,
        _relays: relays,
        seen,
        games,
    }
}

/// The UDP echo harness: the "game" sends a datagram every 20 ms from an
/// unconnected socket (a connected one keeps the source address it chose
/// before Optimize, and the engine drops replies to it), recording when
/// each reply came back and which way.
struct Harness {
    replies: Arc<Mutex<Vec<(Instant, Path)>>>,
    stop: Arc<AtomicBool>,
}

async fn harness() -> Harness {
    let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await.unwrap());
    let replies = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (socket, stop) = (socket.clone(), stop.clone());
        tokio::spawn(async move {
            let mut seq = 0u32;
            while !stop.load(Ordering::Relaxed) {
                seq += 1;
                let _ = socket.send_to(&seq.to_be_bytes(), GAME_SERVER).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
    }
    {
        let (replies, stop) = (replies.clone(), stop.clone());
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while !stop.load(Ordering::Relaxed) {
                let Ok(Ok((n, _))) =
                    tokio::time::timeout(Duration::from_millis(200), socket.recv_from(&mut buf))
                        .await
                else {
                    continue;
                };
                if n < 8 {
                    continue;
                }
                let seen = Ipv4Addr::new(buf[n - 4], buf[n - 3], buf[n - 2], buf[n - 1]);
                let path = if seen == EXIT_EGRESS {
                    Path::Tunnel
                } else {
                    assert_eq!(seen, PLAYER, "unexpected source at the server");
                    Path::Direct
                };
                replies.lock().unwrap().push((Instant::now(), path));
            }
        });
    }
    Harness { replies, stop }
}

impl Harness {
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
                start.elapsed() < timeout,
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

/// One datagram to the server outside the game's ranges: which way?
async fn other_server_path() -> Path {
    let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    socket.send_to(b"other", OTHER_SERVER).await.unwrap();
    let mut buf = [0u8; 64];
    let (n, _) = tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut buf))
        .await
        .expect("other server answers")
        .unwrap();
    let seen = Ipv4Addr::new(buf[n - 4], buf[n - 3], buf[n - 2], buf[n - 1]);
    if seen == EXIT_EGRESS {
        Path::Tunnel
    } else {
        Path::Direct
    }
}

fn kernel_routes() -> String {
    ip::ip(&["route", "show"]).unwrap()
}

fn start_engine(world: &World) -> EngineHandle {
    start_engine_with(world, StabilityMode::Auto)
}

fn start_engine_with(world: &World, stability_mode: StabilityMode) -> EngineHandle {
    engine::start(EngineParts {
        config: ClientConfig {
            // Plain HTTP to a non-loopback host fails ClientConfig::validate
            // (it is only checked when loading a file or by set_config).
            control_url: format!("http://{CONTROL}"),
            // The netsim relays sit on 10.x addresses.
            allow_local_relays: true,
            stability_mode,
            ..ClientConfig::default()
        },
        games: world.games.clone(),
        platform: Arc::new(LinuxPlatform::new(ADAPTER)),
        direct: Arc::new(Fixed(Some(Leg {
            rtt_ms: 300.0,
            loss_pct: 0.0,
            samples: 5,
        }))),
        probes: ProbePlan {
            probes: 3,
            spacing: Duration::from_millis(50),
            timeout: Duration::from_millis(600),
        },
    })
}

fn our_routes() -> Vec<String> {
    kernel_routes()
        .lines()
        .filter(|line| line.contains("proto 80") || line.contains(ADAPTER))
        .map(str::to_string)
        .collect()
}

/// Turns Optimize on and checks the game's traffic, and only it, goes
/// through the tunnel without the harness losing replies for 2 s.
async fn optimize(engine: &EngineHandle, harness: &Harness) {
    let toggled = Instant::now();
    engine.enable(false).await.unwrap();
    assert_eq!(engine.status().mode, Mode::Optimized);
    let first_tunnel = harness.wait_for(Path::Tunnel, Duration::from_secs(2)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let gap = harness.longest_gap(toggled, Instant::now());
    println!(
        "toggle on: first reply through the tunnel {:?} after enable, longest gap {gap:?}",
        first_tunnel - toggled
    );
    assert!(gap < Duration::from_secs(2), "{gap:?} without a reply");
    let routes = kernel_routes();
    assert!(
        routes.contains(&format!("10.3.1.0/24 dev {ADAPTER} proto 80")),
        "{routes}"
    );
    assert_eq!(
        routes.matches(&format!("dev {ADAPTER}")).count(),
        1,
        "only the game's range goes to the adapter: {routes}"
    );
    assert_eq!(other_server_path().await, Path::Direct);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs root and the network namespaces from tools/netsim/client-netsim.sh"]
async fn the_done_when_criteria_on_a_real_kernel() {
    // Refuse to touch the routing table of anything but ppgame.
    assert!(
        nix::net::if_::if_nametoindex("g0").is_ok(),
        "run inside the ppgame namespace (sudo ip netns exec ppgame ...)"
    );
    let _logs =
        pingpath_diagnostics::init_in("netns-test", "0", None, pingpath_diagnostics::Console::Full);
    let _restore = Restore([EXIT, ENTRY_A, ENTRY_B].map(relay_addr).to_vec());
    let world = world().await;
    let engine = start_engine(&world);
    let harness = harness().await;
    harness.wait_for(Path::Direct, Duration::from_secs(2)).await;

    // A game that must never be routed is refused, and nothing is routed.
    let refused = engine.measure("untested-game", REGION).await;
    assert!(
        matches!(refused, Err(EngineError::Refused { .. })),
        "{refused:?}"
    );
    assert!(our_routes().is_empty(), "{:?}", our_routes());

    // Optimize on, off and on again: the game never loses replies for 2 s.
    let measured = engine.measure("test-game", REGION).await.unwrap();
    assert!(measured.worthwhile, "{measured:?}");
    optimize(&engine, &harness).await;
    // Host routes keep the relays the client talks to (both entries; the
    // exit is out of its reach here) on the physical network.
    let routes = kernel_routes();
    for id in [ENTRY_A, ENTRY_B] {
        let host = format!("{} via 10.1.0.1 dev g0 proto 80", relay_addr(id).ip());
        assert!(routes.contains(&host), "{routes}");
    }
    assert!(
        !routes.contains(&relay_addr(EXIT).ip().to_string()),
        "{routes}"
    );
    let toggled = Instant::now();
    engine.disable().await.unwrap();
    harness.wait_for(Path::Direct, Duration::from_secs(2)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let gap = harness.longest_gap(toggled, Instant::now());
    println!("toggle off: longest gap {gap:?}");
    assert!(gap < Duration::from_secs(2), "{gap:?} without a reply");
    assert!(our_routes().is_empty(), "{:?}", our_routes());
    optimize(&engine, &harness).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen_before = world.seen.lock().unwrap().last().copied().unwrap();
    assert_eq!(seen_before.ip(), IpAddr::V4(EXIT_EGRESS));

    // Kill the entry in use: traffic moves to the other entry, same exit,
    // same address at the server.
    let entry = engine
        .status()
        .path
        .unwrap()
        .entry
        .expect("through an entry");
    let entry_id = if entry == format!("relay-{ENTRY_A}") {
        ENTRY_A
    } else {
        ENTRY_B
    };
    let killed = Instant::now();
    kill(entry_id);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let status = engine.status();
    assert_eq!(status.mode, Mode::Optimized, "{status:?}");
    assert_ne!(
        status.path.as_ref().unwrap().entry.as_deref(),
        Some(entry.as_str())
    );
    assert!(
        status
            .notices
            .iter()
            .any(|n| matches!(n.kind, NoticeKind::FailedOver { .. })),
        "{:?}",
        status.notices
    );
    assert_eq!(harness.last_path(), Some(Path::Tunnel));
    let gap = harness.longest_gap(killed - Duration::from_millis(100), Instant::now());
    println!("entry failover: longest gap {gap:?}");
    assert!(
        gap < Duration::from_secs(2),
        "the harness went {gap:?} without a reply"
    );
    assert_eq!(
        world.seen.lock().unwrap().last().copied().unwrap(),
        seen_before
    );

    // A hung engine: the guard thread removes the game route itself.
    let frozen = Instant::now();
    engine.freeze_session(Duration::from_secs(4)).await;
    let direct = harness.wait_for(Path::Direct, Duration::from_secs(3)).await;
    println!("hung engine: direct after {:?}", direct - frozen);
    assert!(direct - frozen < Duration::from_secs(2));
    assert!(!kernel_routes().contains(&format!("dev {ADAPTER}")));
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_ne!(engine.status().mode, Mode::Optimized);

    // Kill every relay while optimized: the routes go and the game
    // connects directly within 2 s.
    optimize(&engine, &harness).await;
    let killed = Instant::now();
    for id in [EXIT, ENTRY_A, ENTRY_B]
        .into_iter()
        .filter(|&id| id != entry_id)
    {
        kill(id);
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
    println!(
        "fallback: first direct reply {:?} after the kill, {:?} after the last tunnel reply",
        direct - killed,
        direct - last_tunnel
    );
    assert!(direct - last_tunnel < Duration::from_secs(2));
    assert!(!kernel_routes().contains(&format!("dev {ADAPTER}")));
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

    // Stopping leaves no routes and no adapter behind.
    engine.shutdown().await;
    assert!(our_routes().is_empty(), "{:?}", our_routes());
    assert!(
        nix::net::if_::if_nametoindex(ADAPTER).is_err(),
        "the adapter is gone"
    );
    let leftovers = LinuxPlatform::new(ADAPTER).cleanup().unwrap();
    assert_eq!((leftovers.adapters, leftovers.routes), (0, 0));
}

/// Runs tools/netsim/client-netsim.sh with `args` (the test runs as root).
fn netsim(args: &[&str]) {
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/netsim/client-netsim.sh"
    );
    let out = std::process::Command::new("bash")
        .arg(script)
        .args(args)
        .output()
        .expect("run client-netsim.sh");
    assert!(
        out.status.success(),
        "client-netsim.sh {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    println!("{}", String::from_utf8_lossy(&out.stdout).trim());
}

/// Random loss on one relay's path, removed when dropped.
struct Loss;

impl Loss {
    fn on(relay: IpAddr, pct: u32) -> Self {
        netsim(&["loss", &relay.to_string(), &pct.to_string()]);
        Loss
    }
}

impl Drop for Loss {
    fn drop(&mut self) {
        netsim(&["loss-off"]);
    }
}

/// Sends `count` numbered datagrams to the game server, one every
/// `every`, and counts how many came back through the tunnel.
async fn delivered(count: u32, every: Duration) -> usize {
    let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await.unwrap());
    let received = Arc::new(Mutex::new(HashSet::new()));
    let reader = {
        let (socket, received) = (socket.clone(), received.clone());
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while let Ok((n, _)) = socket.recv_from(&mut buf).await {
                if n == 8 && buf[4..8] == EXIT_EGRESS.octets() {
                    let seq = u32::from_be_bytes(buf[..4].try_into().unwrap());
                    received.lock().unwrap().insert(seq);
                }
            }
        })
    };
    for seq in 0..count {
        let _ = socket.send_to(&seq.to_be_bytes(), GAME_SERVER).await;
        tokio::time::sleep(every).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    reader.abort();
    received.lock().unwrap().len()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs root and the network namespaces from tools/netsim/client-netsim.sh"]
async fn stability_mode_hides_loss_on_one_entry() {
    // PLAN.md Phase 4's done-when (ADR 010): with 3% random loss each way
    // on the first path's entry, what reaches the game server and comes
    // back loses under 0.3% with Stability mode on. With it off the same
    // loss shows (about 6%), so the test can tell duplication working from
    // a lucky run.
    assert!(
        nix::net::if_::if_nametoindex("g0").is_ok(),
        "run inside the ppgame namespace (sudo ip netns exec ppgame ...)"
    );
    let _logs =
        pingpath_diagnostics::init_in("netns-test", "0", None, pingpath_diagnostics::Console::Full);
    let world = world().await;
    const SENT: u32 = 2000;
    let mut lost = Vec::new();
    for mode in [StabilityMode::On, StabilityMode::Off] {
        let engine = start_engine_with(&world, mode);
        let measured = engine.measure("test-game", REGION).await.unwrap();
        assert!(measured.worthwhile, "{measured:?}");
        engine.enable(false).await.unwrap();
        let entry = engine
            .status()
            .path
            .unwrap()
            .entry
            .expect("through an entry");
        let id = if entry == format!("relay-{ENTRY_A}") {
            ENTRY_A
        } else {
            ENTRY_B
        };
        let loss = Loss::on(relay_addr(id).ip(), 3);
        if mode == StabilityMode::On {
            let started = Instant::now();
            while engine.status().path.unwrap().second_entry.is_none() {
                assert!(started.elapsed() < Duration::from_secs(5), "no second path");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let got = delivered(SENT, Duration::from_millis(5)).await;
        let pct = 100.0 * f64::from(SENT - got as u32) / f64::from(SENT);
        let path = engine.status().path.unwrap();
        println!(
            "stability {mode:?}: {got} of {SENT} came back ({pct:.2}% lost); first path {entry}, second {:?}, extra {} B up, {} B down",
            path.second_entry, path.extra_bytes_up, path.extra_bytes_down
        );
        lost.push(pct);
        drop(loss);
        engine.shutdown().await;
    }
    assert!(lost[0] < 0.3, "{:.2}% lost with Stability mode on", lost[0]);
    assert!(
        lost[1] > 2.0,
        "{:.2}% lost with it off: the loss didn't take effect",
        lost[1]
    );
}
