//! The bufferbloat check end to end on loopback: a relay with a doctor
//! listener, a control plane that issues its tickets, and the client's
//! whole check (docs/decisions/011-home-network-doctor.md). Loopback has
//! no queue to fill, so the grade is the best one; the netns test grades
//! real queues.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use pingpath_client_core::control::ControlClient;
use pingpath_client_core::doctor::bufferbloat::{self, Figures, Grade, Plan};
use pingpath_client_core::doctor::{CheckId, FindingCode, Status};
use pingpath_control::config::{
    Config as ControlConfig, DoctorLimits, RelayEntry, Role as ControlRole, SessionLimits,
};
use pingpath_control::{Control, ControlOptions};
use pingpath_relay::config::{Config as RelayConfig, DoctorListener, Limits, ProbeConfig, Role};
use pingpath_relay::{Relay, RelayOptions};
use pingpath_tunnel::keys::{self, WireGuardKeys};
use pingpath_tunnel::token::Issuer;

const LOCALHOST: Ipv4Addr = Ipv4Addr::LOCALHOST;
const DOCTOR_RELAY: u32 = 3;

fn short_plan() -> Plan {
    Plan {
        idle: Duration::from_millis(1000),
        loaded: Duration::from_millis(1500),
        warmup: Duration::from_millis(300),
        flows: 4,
        ping_interval: Duration::from_millis(50),
        grace: Duration::from_millis(500),
    }
}

struct World {
    _relay: Relay,
    _control: Control,
    url: String,
}

async fn world(doctor: DoctorLimits) -> World {
    let seed = keys::random_32().unwrap();
    let issuer = Issuer::from_seed(&seed).unwrap();
    let relay = Relay::start(RelayOptions {
        config: RelayConfig {
            id: DOCTOR_RELAY,
            name: "near".into(),
            listen: SocketAddr::from((LOCALHOST, 0)),
            http: None,
            roles: vec![Role::Entry],
            private_key_file: None,
            regions: Vec::new(),
            issuer_public_keys: vec![keys::encode(&issuer.public_key())],
            games_dir: None,
            limits: Limits::default(),
            relays: Vec::new(),
            probe: ProbeConfig {
                interval_secs: 0,
                probes_per_round: 0,
            },
            control: None,
            doctor: Some(DoctorListener {
                listen: SocketAddr::from((LOCALHOST, 0)),
            }),
        },
        games: Vec::new(),
        exit_secret: None,
        control_api_key: None,
    })
    .await
    .unwrap();
    let listen = std::net::TcpListener::bind((LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap();
    let entry = |id: u32, name: &str, roles: Vec<ControlRole>, address, doctor| RelayEntry {
        id,
        name: name.into(),
        address,
        regions: if roles.contains(&ControlRole::Exit) {
            vec!["here".into()]
        } else {
            vec![]
        },
        public_key: roles
            .contains(&ControlRole::Exit)
            .then(|| WireGuardKeys::generate().unwrap().public_base64()),
        api_key_sha256: None,
        roles,
        provider: None,
        location: None,
        doctor,
    };
    let control = Control::start(ControlOptions {
        config: ControlConfig {
            listen,
            admin_listen: None,
            issuer_seed_file: "unused".into(),
            games_dir: None,
            client_ip_header: None,
            sessions: SessionLimits::default(),
            doctor,
            crowd: None,
            relays: vec![
                // An exit that runs no tests (and isn't started).
                entry(
                    1,
                    "far",
                    vec![ControlRole::Exit],
                    "127.0.0.1:9".parse().unwrap(),
                    None,
                ),
                entry(
                    DOCTOR_RELAY,
                    "near",
                    vec![ControlRole::Entry],
                    relay.local_addr(),
                    relay.doctor_addr(),
                ),
            ],
        },
        games: Vec::new(),
        issuer_seed: seed,
    })
    .await
    .unwrap();
    World {
        _relay: relay,
        _control: control,
        url: format!("http://{listen}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_check_draws_load_from_the_nearest_test_server_and_grades_it() {
    // Loopback moves gigabytes a second: a budget it can't use up.
    let world = world(DoctorLimits {
        max_bytes: 1 << 40,
        ..DoctorLimits::default()
    })
    .await;
    let control = ControlClient::new(&world.url).unwrap();
    let result = bufferbloat::run(Some(control), &short_plan()).await;
    assert_eq!(result.check, CheckId::Bufferbloat);
    assert_ne!(result.status, Status::Failed, "{:?}", result.error);
    let figures: Figures = serde_json::from_value(result.figures.clone()).unwrap();
    println!("{figures:?}");
    assert_eq!(figures.server, "near");
    assert!(figures.idle.median_ms.is_some_and(|ms| ms < 20.0));
    assert!(figures.download.mbps > 1.0, "{figures:?}");
    assert!(figures.upload.mbps > 1.0, "{figures:?}");
    assert!(figures.download.latency.pings > 5);
    // Loopback has no queue worth the name.
    assert!(figures.grade <= Grade::B, "{figures:?}");
    assert_eq!(result.findings[0].code, FindingCode::BufferbloatGrade);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_network_over_its_limit_is_told_when_to_come_back() {
    let world = world(DoctorLimits {
        per_network_per_hour: 1,
        max_bytes: 1 << 40,
        ..DoctorLimits::default()
    })
    .await;
    let control = ControlClient::new(&world.url).unwrap();
    let first = bufferbloat::run(Some(control.clone()), &short_plan()).await;
    assert_ne!(first.status, Status::Failed, "{:?}", first.error);
    let second = bufferbloat::run(Some(control), &short_plan()).await;
    assert_eq!(second.status, Status::Unavailable);
    assert_eq!(second.findings[0].code, FindingCode::TestLimitReached);
    assert!(second.findings[0].params["minutes"].as_u64().unwrap() <= 60);
}

#[tokio::test]
async fn without_a_control_plane_there_is_no_test_server() {
    let result = bufferbloat::run(None, &short_plan()).await;
    assert_eq!(result.status, Status::Unavailable);
    assert_eq!(result.findings[0].code, FindingCode::NoTestServer);
}
