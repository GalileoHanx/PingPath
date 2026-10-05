//! The Windows backend against a real Windows: the Wintun adapter, the
//! route table, and fail-open when the service is killed. These change the
//! machine's adapters and routes, so they are ignored by default. CI runs
//! them on windows-latest, as administrator, with `PINGPATH_WINTUN_DLL`
//! set to wintun.dll's absolute path:
//!
//! ```text
//! cargo test -p pingpath-client-core --test windows_backend -- --ignored
//! ```
//!
//! They only touch addresses that are safe to route on a CI runner: the
//! benchmarking range 198.18.0.0/15 to the adapter, and host routes for
//! 1.1.1.1 and 1.0.0.1 through the runner's own gateway.

#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError, mpsc as std_mpsc};
use std::thread;
use std::time::{Duration, Instant};

use ipnet::IpNet;
use pingpath_client_core::net::route_table::{HOST_ROUTE_METRIC, Ledger, RouteRow};
use pingpath_client_core::net::windows::{self, WindowsPlatform};
use pingpath_client_core::net::{AdapterConfig, InterfaceId, Platform, Route};
use pingpath_tunnel::packet;

const DLL_ENV: &str = "PINGPATH_WINTUN_DLL";
/// Set in the child process of `routes_vanish_when_the_service_is_killed`.
const CHILD_ENV: &str = "PINGPATH_WINDOWS_BACKEND_CHILD";
const CONFIG: AdapterConfig = AdapterConfig {
    address: Ipv4Addr::new(192, 0, 0, 8),
    mtu: 1380,
};

/// The tests share one adapter GUID, so they take turns.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn ledger_path(dir: &Path) -> PathBuf {
    dir.join("state").join("routes.json")
}

fn platform(dir: &Path) -> WindowsPlatform {
    let dll = std::env::var_os(DLL_ENV)
        .unwrap_or_else(|| panic!("set {DLL_ENV} to the absolute path of wintun.dll"));
    WindowsPlatform::new(Path::new(&dll), ledger_path(dir)).expect("wintun.dll loads")
}

fn routes_to(destination: IpNet) -> Vec<RouteRow> {
    windows::routes()
        .unwrap()
        .into_iter()
        .filter(|row| row.destination == destination)
        .collect()
}

/// Waits up to `limit` for `done`, checking every 50 ms.
fn eventually(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "needs administrator rights and PINGPATH_WINTUN_DLL (CI on windows-latest)"]
fn the_adapter_receives_only_routed_traffic() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let platform = platform(dir.path());
    runtime().block_on(async {
        let (adapter, mut channels) = platform.create_adapter(&CONFIG).unwrap();
        let interface = adapter.interface();
        assert_eq!(windows::find_adapter().unwrap(), Some(interface));

        let prefix: IpNet = "198.18.0.0/24".parse().unwrap();
        let route = Route::Adapter { prefix, interface };
        platform.add_route(&route).unwrap();
        assert!(
            routes_to(prefix)
                .iter()
                .any(|row| row.interface == interface.0),
            "the game route is in the table"
        );

        // A plain socket, as a game would use: the system routes the
        // datagram to the adapter. The adapter can take a moment to come
        // up after creation, so keep sending until one arrives.
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        let routed: SocketAddrV4 = "198.18.0.1:9".parse().unwrap();
        let unrouted: SocketAddrV4 = "198.19.0.1:9".parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let source = loop {
            assert!(
                Instant::now() < deadline,
                "no packet to {routed} reached the adapter"
            );
            socket.send_to(b"routed", routed).unwrap();
            let wait = tokio::time::timeout(Duration::from_millis(200), channels.incoming.recv());
            if let Ok(Some(bytes)) = wait.await {
                if let Ok(udp) = packet::parse_udp(&bytes) {
                    if udp.dst == routed && udp.payload == b"routed" {
                        break udp.src;
                    }
                }
            }
        };
        assert_eq!(
            *source.ip(),
            CONFIG.address,
            "sent from the adapter's address"
        );

        // An address outside the route goes the normal way: nothing for it
        // reaches the adapter.
        for _ in 0..5 {
            socket.send_to(b"direct", unrouted).unwrap();
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while let Ok(Some(bytes)) =
            tokio::time::timeout_at(deadline, channels.incoming.recv()).await
        {
            if let Ok(udp) = packet::parse_udp(&bytes) {
                assert_ne!(
                    udp.dst, unrouted,
                    "a packet for an unrouted address reached the adapter"
                );
            }
        }

        platform.remove_route(&route).unwrap();
        assert!(
            !routes_to(prefix)
                .iter()
                .any(|row| row.interface == interface.0),
            "the game route is gone"
        );
        // Removing it again is not an error.
        platform.remove_route(&route).unwrap();

        drop(adapter);
        assert!(
            eventually(Duration::from_secs(5), || windows::find_adapter()
                .unwrap()
                .is_none()),
            "the adapter is gone after it is dropped"
        );
    });
}

/// Adds a host route with `route.exe`, as another program would, and
/// deletes it when dropped.
struct ForeignRoute {
    destination: Ipv4Addr,
    gateway: IpAddr,
}

impl ForeignRoute {
    fn add(destination: Ipv4Addr, gateway: IpAddr, metric: u32, index: u32) -> Self {
        let status = Command::new("route")
            .args(["add", &destination.to_string(), "mask", "255.255.255.255"])
            .arg(gateway.to_string())
            .args(["metric", &metric.to_string(), "if", &index.to_string()])
            .status()
            .unwrap();
        assert!(status.success(), "route add failed");
        Self {
            destination,
            gateway,
        }
    }
}

impl Drop for ForeignRoute {
    fn drop(&mut self) {
        let _ = Command::new("route")
            .args([
                "delete",
                &self.destination.to_string(),
                "mask",
                "255.255.255.255",
            ])
            .arg(self.gateway.to_string())
            .status();
    }
}

fn same_routes(a: &[RouteRow], b: &[RouteRow]) -> bool {
    a.iter().all(|x| b.iter().any(|y| x.same_route(y)))
}

#[test]
#[ignore = "needs administrator rights and PINGPATH_WINTUN_DLL (CI on windows-latest)"]
fn host_routes_follow_the_ledger() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let platform = platform(dir.path());

    let relay: IpAddr = "1.1.1.1".parse().unwrap();
    let relay_host: IpNet = IpNet::from(relay);
    let via = platform.best_route(relay).unwrap();
    let gateway = via
        .gateway
        .expect("the runner reaches the internet through a gateway");
    let defaults_before = routes_to("0.0.0.0/0".parse().unwrap());
    assert!(
        !defaults_before.is_empty(),
        "the runner has a default route"
    );

    // Another program's host route through the same gateway, with its own
    // metric, made before anything of ours.
    let foreign_destination = Ipv4Addr::new(1, 0, 0, 1);
    let foreign_host = IpNet::from(IpAddr::V4(foreign_destination));
    let index = windows::interface_index(via.interface).unwrap();
    let _foreign = ForeignRoute::add(foreign_destination, gateway, 7, index);
    let foreign = routes_to(foreign_host)
        .into_iter()
        .find(|row| row.interface == via.interface.0)
        .expect("route.exe added the foreign route");
    assert_ne!(foreign.metric, HOST_ROUTE_METRIC);

    // Ours: added with the marker metric and recorded in the ledger first.
    let route = Route::Host {
        address: relay,
        via,
    };
    platform.add_route(&route).unwrap();
    let ours = RouteRow {
        interface: via.interface.0,
        destination: relay_host,
        next_hop: Some(gateway),
        metric: HOST_ROUTE_METRIC,
    };
    assert!(
        routes_to(relay_host).contains(&ours),
        "our host route is in the table"
    );
    assert!(Ledger::load(ledger_path(dir.path())).unwrap().owns(&ours));
    // The physical route is still found while our host route covers it.
    assert_eq!(platform.best_route(relay).unwrap(), via);

    platform.remove_route(&route).unwrap();
    assert!(
        !routes_to(relay_host).contains(&ours),
        "our host route is gone"
    );
    assert!(
        Ledger::load(ledger_path(dir.path()))
            .unwrap()
            .records()
            .is_empty()
    );

    // Asking for a host route that the foreign route already provides
    // uses it, and neither adding nor removing touches it.
    let over_foreign = Route::Host {
        address: IpAddr::V4(foreign_destination),
        via,
    };
    platform.add_route(&over_foreign).unwrap();
    platform.remove_route(&over_foreign).unwrap();
    assert!(
        routes_to(foreign_host).contains(&foreign),
        "the foreign route is untouched"
    );

    // A crash with our host route in place: the next run's cleanup removes
    // it from the ledger's record. A record that matches the foreign route
    // in everything but the metric must not remove it.
    platform.add_route(&route).unwrap();
    drop(platform);
    let mut ledger = Ledger::load(ledger_path(dir.path())).unwrap();
    ledger
        .insert(RouteRow {
            metric: HOST_ROUTE_METRIC,
            ..foreign
        })
        .unwrap();
    let platform = self::platform(dir.path());
    let report = platform.cleanup().unwrap();
    assert_eq!(report.routes, 1);
    assert!(
        !routes_to(relay_host).contains(&ours),
        "cleanup removed our host route"
    );
    assert!(
        routes_to(foreign_host).contains(&foreign),
        "cleanup left the foreign route"
    );
    let defaults_after = routes_to("0.0.0.0/0".parse().unwrap());
    assert!(
        same_routes(&defaults_before, &defaults_after),
        "cleanup left the default routes: {defaults_before:?} -> {defaults_after:?}"
    );
    assert!(
        Ledger::load(ledger_path(dir.path()))
            .unwrap()
            .records()
            .is_empty()
    );
}

/// The child half of `routes_vanish_when_the_service_is_killed`: creates
/// the adapter and a game route, says READY, and waits to be killed. Does
/// nothing when run on its own.
#[test]
#[ignore = "run by routes_vanish_when_the_service_is_killed"]
fn crashing_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let platform = platform(dir.path());
    runtime().block_on(async {
        let (adapter, _channels) = platform.create_adapter(&CONFIG).unwrap();
        let interface = adapter.interface();
        platform
            .add_route(&Route::Adapter {
                prefix: "198.18.1.0/24".parse().unwrap(),
                interface,
            })
            .unwrap();
        let mut stdout = std::io::stdout();
        writeln!(stdout, "READY {}", interface.0).unwrap();
        stdout.flush().unwrap();
        std::future::pending::<()>().await;
    });
}

#[test]
#[ignore = "needs administrator rights and PINGPATH_WINTUN_DLL (CI on windows-latest)"]
fn routes_vanish_when_the_service_is_killed() {
    let _serial = serial();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "crashing_child",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (ready_tx, ready) = std_mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Some(luid) = line.strip_prefix("READY ") {
                let _ = ready_tx.send(luid.trim().parse::<u64>().ok());
            }
        }
    });
    let interface = match ready.recv_timeout(Duration::from_secs(60)) {
        Ok(Some(luid)) => InterfaceId(luid),
        other => {
            let _ = child.kill();
            panic!("the child didn't get ready: {other:?}");
        }
    };
    let prefix: IpNet = "198.18.1.0/24".parse().unwrap();
    let routed = || {
        routes_to(prefix)
            .iter()
            .any(|row| row.interface == interface.0)
    };
    assert!(routed(), "the child's game route is in the table");

    // TerminateProcess: no destructor runs, as in a crash.
    child.kill().unwrap();
    let killed = Instant::now();
    child.wait().unwrap();
    assert!(
        eventually(Duration::from_secs(2), || !routed()),
        "the game route was still there 2 s after the kill"
    );
    eprintln!(
        "game route gone {} ms after the kill; adapter still listed: {:?}",
        killed.elapsed().as_millis(),
        windows::find_adapter().unwrap()
    );

    // Leave nothing for the next test: cleanup removes a stale adapter.
    let dir = tempfile::tempdir().unwrap();
    let report = platform(dir.path()).cleanup().unwrap();
    eprintln!("cleanup after the kill: {report:?}");
    assert_eq!(windows::find_adapter().unwrap(), None);
}
