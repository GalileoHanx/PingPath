//! PLAN.md Phase 5's done-when for the bufferbloat test
//! (docs/decisions/011-home-network-doctor.md, "The test"): on a line with
//! a queue of known length (`client-netsim.sh shape`), the grade must be
//! within one letter of the one the queue implies, and of a reference
//! measured on the same line: `ping` ten times a second while `iperf3`
//! downloads and then uploads over eight streams, graded on the same
//! thresholds. Without iperf3 and ping the reference is skipped, unless
//! PINGPATH_REQUIRE_REFERENCE is set (CI sets it).

use std::net::{Ipv4Addr, SocketAddr};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pingpath_client_core::doctor::bufferbloat::{self, Grade, Plan};
use pingpath_relay::config::{Config as RelayConfig, DoctorListener, Limits, ProbeConfig, Role};
use pingpath_relay::{Relay, RelayOptions};
use pingpath_tunnel::keys;
use pingpath_tunnel::ticket::{self, Ticket};
use pingpath_tunnel::token::{Issuer, now_unix};

use super::netsim;
use super::support::InNetns;

/// Entry A's address in pprelays; this test starts a relay of its own
/// there, on other ports.
const SERVER: Ipv4Addr = Ipv4Addr::new(10, 2, 1, 11);
const DOCTOR: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(SERVER), 8443);
const RELAY_ID: u32 = 9;
const RATE: &str = "20mbit";
const IPERF_PORT: &str = "5201";

/// The line's shaping, removed when dropped.
struct Shaped;

impl Shaped {
    fn on(queue_ms: u32) -> Self {
        netsim(&["shape", RATE, &queue_ms.to_string()]);
        Shaped
    }
}

impl Drop for Shaped {
    fn drop(&mut self) {
        netsim(&["shape-off"]);
    }
}

/// A child process, killed when dropped.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn doctor_relay(issuer: [u8; 32]) -> InNetns {
    InNetns::spawn("pprelays", move || async move {
        let relay = Relay::start(RelayOptions {
            config: RelayConfig {
                id: RELAY_ID,
                name: "doctor".into(),
                listen: SocketAddr::new(SERVER.into(), 51990),
                http: None,
                roles: vec![Role::Entry],
                private_key_file: None,
                regions: Vec::new(),
                issuer_public_keys: vec![keys::encode(&issuer)],
                games_dir: None,
                limits: Limits::default(),
                relays: Vec::new(),
                probe: ProbeConfig {
                    interval_secs: 0,
                    probes_per_round: 0,
                },
                control: None,
                doctor: Some(DoctorListener { listen: DOCTOR }),
            },
            games: Vec::new(),
            exit_secret: None,
            control_api_key: None,
        })
        .await
        .expect("start the doctor relay");
        std::future::pending::<()>().await;
        drop(relay);
    })
    .expect("run in pprelays")
}

fn ticket(issuer: &Issuer) -> Vec<u8> {
    let now = now_unix();
    issuer.sign_ticket(&Ticket {
        id: keys::random_32().unwrap()[..ticket::ID_LEN]
            .try_into()
            .unwrap(),
        relay_id: RELAY_ID,
        issued_at: now,
        expires_at: now + 120,
        max_bytes: 1 << 34,
    })
}

/// Shorter than a player's test, with the same shape.
fn plan() -> Plan {
    Plan {
        idle: Duration::from_secs(3),
        loaded: Duration::from_secs(6),
        warmup: Duration::from_secs(1),
        flows: 8,
        ping_interval: Duration::from_millis(100),
        grace: Duration::from_secs(2),
    }
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    Some(values[values.len() / 2])
}

/// The round trips of `count` pings to the server, ten a second.
fn ping(count: u32) -> Vec<f64> {
    let out = Command::new("ping")
        .args([
            "-n",
            "-i",
            "0.1",
            "-W",
            "3",
            "-c",
            &count.to_string(),
            &SERVER.to_string(),
        ])
        .output()
        .expect("run ping");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split("time=").nth(1)?.split(' ').next()?.parse().ok())
        .collect()
}

fn installed(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// The reference: ping's median at rest, then during an iperf3 download
/// and an upload over eight streams, graded on the larger increase (or F
/// when over half the pings under load were lost). `None` without the
/// tools.
fn reference() -> Option<(Grade, f64)> {
    if !installed("iperf3") || !installed("ping") {
        return None;
    }
    let _server = Running(
        Command::new("ip")
            .args([
                "netns",
                "exec",
                "pprelays",
                "iperf3",
                "-s",
                "-B",
                &SERVER.to_string(),
                "-p",
                IPERF_PORT,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start iperf3 -s"),
    );
    std::thread::sleep(Duration::from_millis(500));
    let idle = median(ping(30)).expect("pings answer at rest");
    let mut worst = 0.0f64;
    for download in [true, false] {
        let mut args = vec!["-c", "10.2.1.11", "-p", IPERF_PORT, "-P", "8", "-t", "8"];
        if download {
            args.push("-R");
        }
        let mut client = Running(
            Command::new("iperf3")
                .args(&args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start iperf3 -c"),
        );
        // Past TCP's start, as the doctor's own warmup.
        std::thread::sleep(Duration::from_secs(1));
        let rtts = ping(60);
        let _ = client.0.wait();
        let increase = if rtts.len() < 30 {
            f64::INFINITY
        } else {
            median(rtts).unwrap() - idle
        };
        worst = worst.max(increase);
    }
    Some((Grade::from_increase(worst), worst))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs root and the network namespaces from tools/netsim/client-netsim.sh"]
async fn the_bufferbloat_grade_follows_the_queue() {
    let issuer = Issuer::from_seed(&keys::random_32().unwrap()).unwrap();
    let _relay = doctor_relay(issuer.public_key());
    let deadline = Instant::now() + Duration::from_secs(5);
    while tokio::net::TcpStream::connect(DOCTOR).await.is_err() {
        assert!(
            Instant::now() < deadline,
            "the doctor listener never came up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let need_reference = std::env::var_os("PINGPATH_REQUIRE_REFERENCE").is_some();

    // A queue's length is about the latency it adds when full.
    for (queue_ms, implied) in [(15, Grade::A), (120, Grade::C), (600, Grade::F)] {
        let _shaped = Shaped::on(queue_ms);
        let (idle, download, upload) = bufferbloat::measure(DOCTOR, &ticket(&issuer), &plan())
            .await
            .expect("the bufferbloat test runs");
        let figures = bufferbloat::grade("doctor".into(), idle, download, upload);
        let reference = tokio::task::spawn_blocking(reference).await.unwrap();
        println!(
            "{queue_ms} ms queue at {RATE}: grade {} (+{:?} ms down at {} Mbit/s, +{:?} ms up at {} Mbit/s); \
             implied {}; reference {}",
            figures.grade.letter(),
            figures.download.increase_ms,
            figures.download.mbps,
            figures.upload.increase_ms,
            figures.upload.mbps,
            implied.letter(),
            reference.map_or("not run".to_string(), |(grade, ms)| format!(
                "{} (+{ms:.0} ms)",
                grade.letter()
            )),
        );
        assert!(
            figures.grade.distance(implied) <= 1,
            "a {queue_ms} ms queue graded {}, not within a letter of {}",
            figures.grade.letter(),
            implied.letter()
        );
        match reference {
            Some((grade, _)) => assert!(
                figures.grade.distance(grade) <= 1,
                "a {queue_ms} ms queue graded {}, the reference {}",
                figures.grade.letter(),
                grade.letter()
            ),
            None => assert!(!need_reference, "the reference needs iperf3 and ping"),
        }
    }
}
