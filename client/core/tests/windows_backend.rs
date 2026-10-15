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
//! 1.1.1.1 and 1.0.0.1 on the runner's own interface (through its gateway,
//! and for a moment on-link).

#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError, mpsc as std_mpsc};
use std::thread;
use std::time::{Duration, Instant};

use ipnet::IpNet;
use pingpath_client_core::net::route_table::{HOST_ROUTE_METRIC, Ledger, RouteRow};
use pingpath_client_core::net::windows::{self, WindowsPlatform};
use pingpath_client_core::net::{AdapterConfig, InterfaceId, NextHop, Platform, Route};
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

        // IPv6 is never routed to the adapter: at most the link-local and
        // multicast routes Windows gives every interface may come back.
        let link_scoped: [IpNet; 2] = ["fe80::/10".parse().unwrap(), "ff00::/8".parse().unwrap()];
        let ipv6_routes: Vec<RouteRow> = windows::routes()
            .unwrap()
            .into_iter()
            .filter(|row| row.interface == interface.0 && row.destination.addr().is_ipv6())
            .filter(|row| !link_scoped.iter().any(|net| net.contains(&row.destination)))
            .collect();
        assert!(
            ipv6_routes.is_empty(),
            "IPv6 routes on the adapter: {ipv6_routes:?}"
        );

        let prefix: IpNet = "198.18.0.0/24".parse().unwrap();
        let route = Route::Adapter { prefix, interface };
        platform.add_route(&route).unwrap();
        assert!(
            routes_to(prefix)
                .iter()
                .any(|row| row.interface == interface.0),
            "the game route is in the table"
        );

        // Windows sends its own traffic on a new interface (IGMP, mDNS,
        // LLMNR, SSDP, NetBIOS), so only this test's packets are judged:
        // each payload starts with a marker no other sender uses, then says
        // where it was sent. Anything without the marker is ignored.
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let marker = format!("pingpath-test-{}-{started:x}", std::process::id());
        let to_routed = format!("{marker} routed");
        let to_unrouted = format!("{marker} unrouted");

        // A plain socket, as a game would use: the system routes the
        // datagram to the adapter. create_adapter returns once the address
        // is usable, so the first one should arrive; resending covers a
        // slow first route lookup.
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        let routed: SocketAddrV4 = "198.18.0.1:9".parse().unwrap();
        let unrouted: SocketAddrV4 = "198.19.0.1:9".parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut send_error = None;
        let source = loop {
            assert!(
                Instant::now() < deadline,
                "no tagged packet to {routed} reached the adapter (last send error: {send_error:?})"
            );
            if let Err(e) = socket.send_to(to_routed.as_bytes(), routed) {
                send_error = Some(e);
            }
            let wait = tokio::time::timeout(Duration::from_millis(200), channels.incoming.recv());
            match wait.await {
                Ok(Some(bytes)) => {
                    if let Ok(udp) = packet::parse_udp(&bytes) {
                        if udp.payload == to_routed.as_bytes() {
                            assert_eq!(
                                udp.dst, routed,
                                "the routed packet arrived for another address"
                            );
                            break udp.src;
                        }
                    }
                }
                Ok(None) => panic!("the adapter stopped delivering packets"),
                Err(_) => {}
            }
        };
        assert_eq!(
            *source.ip(),
            CONFIG.address,
            "sent from the adapter's address"
        );

        // The way back, as the engine delivers the exit's replies: a packet
        // written to the adapter, from the server to the adapter's address,
        // reaches the game's socket. Windows takes a packet in only on the
        // interface that has its destination address, so this fails if the
        // writer, the address or the adapter's receive side is wrong. The
        // reply is written again until it arrives, like the request above.
        let reply = format!("{marker} reply");
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 2048];
        loop {
            assert!(
                Instant::now() < deadline,
                "the reply written to the adapter never reached the socket"
            );
            channels
                .outgoing
                .send(packet::build_udp(routed, source, reply.as_bytes()))
                .await
                .expect("the adapter's writer is running");
            // A timeout, or an ICMP error left by an earlier send: try again.
            if let Ok((n, from)) = socket.recv_from(&mut buf) {
                if &buf[..n] == reply.as_bytes() {
                    assert_eq!(
                        from,
                        SocketAddr::V4(routed),
                        "the reply arrived from the server's address"
                    );
                    break;
                }
            }
        }

        // An address outside the route goes the normal way: a tagged
        // packet for it never reaches the adapter.
        for _ in 0..5 {
            socket.send_to(to_unrouted.as_bytes(), unrouted).unwrap();
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        loop {
            match tokio::time::timeout_at(deadline, channels.incoming.recv()).await {
                Ok(Some(bytes)) => {
                    let Ok(udp) = packet::parse_udp(&bytes) else {
                        continue;
                    };
                    // Late copies of the routed packet are fine.
                    if udp.payload.starts_with(marker.as_bytes()) {
                        assert!(
                            udp.payload == to_routed.as_bytes(),
                            "a tagged packet outside the route reached the adapter: {:?} to {}",
                            String::from_utf8_lossy(udp.payload),
                            udp.dst
                        );
                    }
                }
                Ok(None) => panic!("the adapter stopped delivering packets"),
                // A second with none of them.
                Err(_) => break,
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

    // Game routes go on the PingPath adapter only, never on another
    // interface.
    let misplaced = Route::Adapter {
        prefix: "198.18.2.0/24".parse().unwrap(),
        interface: via.interface,
    };
    assert_eq!(
        platform.add_route(&misplaced).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert!(routes_to("198.18.2.0/24".parse().unwrap()).is_empty());

    // The tunnel socket can be pinned to that interface and still send.
    runtime().block_on(async {
        let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
        platform.prepare_socket(&socket, via).unwrap();
        socket.send_to(b"pinned", (relay, 9)).await.unwrap();
    });

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

    // The check above can't tell the physical route from ours, which has
    // the same next hop. A host route of ours that points elsewhere, as
    // one does once the player has moved to another network, can: here an
    // on-link one on the same interface, which is what the system itself
    // would now answer for the relay.
    let elsewhere = Route::Host {
        address: relay,
        via: NextHop {
            gateway: None,
            interface: via.interface,
        },
    };
    platform.add_route(&elsewhere).unwrap();
    let found = platform.best_route(relay);
    platform.remove_route(&elsewhere).unwrap();
    assert_eq!(
        found.unwrap(),
        via,
        "the physical route, not our host route that points elsewhere"
    );
    assert!(
        !routes_to(relay_host)
            .iter()
            .any(|row| row.metric == HOST_ROUTE_METRIC),
        "our host routes are gone"
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
        writeln!(stdout, "\nREADY {}", interface.0).unwrap();
        stdout.flush().unwrap();
        std::future::pending::<()>().await;
    });
}

/// The crashing child, killed when dropped: a failure before the test
/// kills it would otherwise leave it running with its adapter, and the
/// tests after this one would meet that adapter.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "needs administrator rights and PINGPATH_WINTUN_DLL (CI on windows-latest)"]
fn routes_vanish_when_the_service_is_killed() {
    let _serial = serial();
    let spawned = Command::new(std::env::current_exe().unwrap())
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
    let mut child = KillOnDrop(spawned);
    let stdout = child.0.stdout.take().unwrap();
    let (ready_tx, ready) = std_mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            // With one test thread, libtest prints "test crashing_child ... "
            // before the test runs, so READY can share its line.
            if let Some((_, luid)) = line.split_once("READY ") {
                let _ = ready_tx.send(luid.trim().parse::<u64>().ok());
            }
        }
    });
    let interface = match ready.recv_timeout(Duration::from_secs(60)) {
        Ok(Some(luid)) => InterfaceId(luid),
        other => panic!("the child didn't get ready: {other:?}"),
    };
    let prefix: IpNet = "198.18.1.0/24".parse().unwrap();
    let routed = || {
        routes_to(prefix)
            .iter()
            .any(|row| row.interface == interface.0)
    };
    assert!(routed(), "the child's game route is in the table");

    // TerminateProcess: no destructor runs, as in a crash. The 2 s count
    // from the kill, not from when the wait for the process returns.
    child.0.kill().unwrap();
    let killed = Instant::now();
    child.0.wait().unwrap();
    assert!(
        eventually(
            Duration::from_secs(2).saturating_sub(killed.elapsed()),
            || !routed()
        ),
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
    assert!(
        eventually(Duration::from_secs(5), || windows::find_adapter()
            .unwrap()
            .is_none()),
        "the stale adapter is gone after cleanup"
    );
}

#[test]
#[ignore = "needs administrator rights and PINGPATH_WINTUN_DLL (CI on windows-latest)"]
fn uninstall_removes_the_wintun_driver() {
    // PLAN.md Phase 3: uninstall leaves no services behind. Making the
    // adapter installs Wintun's driver (a kernel driver service named
    // wintun); with no Wintun adapter left, removing it must take the
    // service away. (On a PC where another product's Wintun adapter is in
    // use, Wintun keeps the driver and remove_driver says so.)
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let platform = platform(dir.path());
    runtime().block_on(async {
        let (adapter, _channels) = platform.create_adapter(&CONFIG).unwrap();
        drop(adapter);
    });
    let driver_service = || {
        Command::new("sc.exe")
            .args(["query", "wintun"])
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    };
    assert!(driver_service(), "making the adapter installs the driver");
    assert!(
        platform.remove_driver().unwrap(),
        "no Wintun adapter is left, so the driver goes"
    );
    assert!(
        eventually(Duration::from_secs(5), || !driver_service()),
        "the wintun driver service is still registered"
    );
}

/// The network doctor's background check reads the kernel's network
/// events (docs/decisions/011-home-network-doctor.md): this process's UDP
/// out to the internet, and the DNS answers that come back, must be
/// counted under its ID, each in its own direction; what it sends itself
/// over loopback never crosses the line and must not be.
#[test]
#[ignore = "needs administrator rights (CI on windows-latest)"]
fn the_network_trace_counts_this_process_traffic() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let to_self = receiver.local_addr().unwrap();
    // TEST-NET-1: routed out and dropped, but sent all the same.
    let to_internet = SocketAddr::from((Ipv4Addr::new(192, 0, 2, 1), 9));
    // Answers from the internet, for the received direction.
    let resolver = SocketAddr::from((Ipv4Addr::new(1, 1, 1, 1), 53));
    // A query for www.google.com's A record.
    let mut query = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in ["www", "google", "com"] {
        query.push(label.len() as u8);
        query.extend_from_slice(label.as_bytes());
    }
    query.extend_from_slice(&[0, 0, 1, 0, 1]);
    let sender = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let traffic = {
        let stop = stop.clone();
        thread::spawn(move || {
            // Bytes over loopback, out to the internet, and answered.
            let (mut looped, mut out, mut answered) = (0u64, 0u64, 0u64);
            let mut buffer = [0u8; 2000];
            receiver.set_nonblocking(true).unwrap();
            sender.set_nonblocking(true).unwrap();
            let mut round = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if sender.send_to(&[7u8; 1000], to_self).is_ok() {
                    looped += 1000;
                }
                if sender.send_to(&[7u8; 1000], to_internet).is_ok() {
                    out += 1000;
                }
                if round % 10 == 0 && sender.send_to(&query, resolver).is_ok() {
                    out += query.len() as u64;
                }
                round += 1;
                while receiver.recv_from(&mut buffer).is_ok() {}
                while let Ok((n, from)) = sender.recv_from(&mut buffer) {
                    if from == resolver {
                        answered += n as u64;
                    }
                }
                thread::sleep(Duration::from_millis(5));
            }
            (looped, out, answered)
        })
    };
    let usage = pingpath_client_core::doctor::background::sample(Duration::from_secs(4))
        .expect("the trace session starts as administrator");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (looped, out, answered) = traffic.join().unwrap();
    let ours = usage.get(&std::process::id()).cloned().unwrap_or_default();
    println!(
        "this process: {ours:?} (sent {out} bytes out, {looped} to itself, {answered} answered); \
         {} processes seen",
        usage.len()
    );
    assert!(out > 0, "nothing could be sent out");
    assert!(answered > 0, "the resolver never answered");
    // The trace starts a moment after the traffic does and stops before
    // it: most of it, not all; and none of what went over loopback.
    assert!(ours.sent >= out / 2, "{ours:?} of {out} out");
    assert!(
        ours.sent <= out + 2000,
        "loopback counted as sent: {ours:?}"
    );
    assert!(
        ours.received >= answered / 2,
        "{ours:?} of {answered} answered"
    );
    assert!(
        ours.received <= answered + 2000,
        "loopback counted as received: {ours:?}"
    );
}
