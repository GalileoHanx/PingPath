//! The control plane with real relays on loopback: a client gets a session
//! over HTTP, reaches a UDP echo server through the entry and exit with
//! that token, renews it without losing the session, and the relays'
//! latency reports (to each other, and from the exit to its game region)
//! show up in `/v1/latency`.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use base64::Engine;
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
use pingpath_tunnel::client::ClientSession;
use pingpath_tunnel::keys::{self, WireGuardKeys};
use pingpath_tunnel::token::Issuer;
use serde_json::{Value, json};
use tokio::net::UdpSocket;

const LOCALHOST: Ipv4Addr = Ipv4Addr::LOCALHOST;
const REGION: &str = "here";
const TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_ID: u32 = 1;
const ENTRY_ID: u32 = 2;

async fn echo_server() -> SocketAddrV4 {
    let socket = UdpSocket::bind((LOCALHOST, 0)).await.unwrap();
    let SocketAddr::V4(addr) = socket.local_addr().unwrap() else {
        unreachable!()
    };
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = socket.recv_from(&mut buf).await {
            let _ = socket.send_to(&buf[..n], from).await;
        }
    });
    addr
}

/// A game whose servers are on loopback (which the validator rightly
/// refuses in real definitions).
fn game(
    id: &str,
    status: SupportStatus,
    port: u16,
    ping_targets: Vec<PingTarget>,
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
                first: port,
                last: port,
            }],
            tcp: vec![],
        },
        sdr_app_id: None,
        regions: vec![Region {
            id: REGION.into(),
            name: "This machine".into(),
            server_ranges: vec!["127.0.0.0/8".parse().unwrap()],
            ping_targets,
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
    relays: Vec<Peer>,
    control_url: &str,
) -> RelayConfig {
    RelayConfig {
        id,
        name: format!("relay-{id}"),
        listen: SocketAddr::from((LOCALHOST, 0)),
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

/// One HTTP request from a blocking thread: status and JSON body.
async fn http(
    method: &'static str,
    url: String,
    body: Option<Value>,
    bearer: Option<String>,
) -> (u16, Value) {
    tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .build()
            .into();
        let mut response = match (method, body) {
            ("GET", _) => agent.get(&url).call(),
            (_, body) => {
                let mut request = agent.post(&url).header("Content-Type", "application/json");
                if let Some(key) = &bearer {
                    request = request.header("Authorization", &format!("Bearer {key}"));
                }
                request.send(
                    body.map(|b| b.to_string())
                        .unwrap_or_else(|| "{not json".into()),
                )
            }
        }
        .unwrap();
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_gets_a_session_from_the_control_plane_and_uses_it() {
    let issuer_seed = keys::random_32().unwrap();
    let issuer = Issuer::from_seed(&issuer_seed).unwrap().public_key();
    let exit_secret = keys::random_32().unwrap();
    let exit_public = WireGuardKeys::from_secret(exit_secret).public_base64();
    let entry_api_key = hex(&keys::random_32().unwrap());
    let exit_api_key = hex(&keys::random_32().unwrap());
    // The game region's ping target: a TCP port the exit times connections to.
    let ping_listener = std::net::TcpListener::bind((LOCALHOST, 0)).unwrap();
    let ping_target = PingTarget::Tcp(ping_listener.local_addr().unwrap());
    let echo = echo_server().await;
    let games = vec![
        game(
            "test-game",
            SupportStatus::Supported,
            echo.port(),
            vec![ping_target],
        ),
        game(
            "untested-game",
            SupportStatus::Untested,
            echo.port(),
            vec![],
        ),
    ];

    // The relays report to the control plane, so pick its port first.
    let control_listen = std::net::TcpListener::bind((LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap();
    let control_url = format!("http://{control_listen}");
    let exit = Relay::start(RelayOptions {
        config: relay_config(EXIT_ID, vec![Role::Exit], issuer, vec![], &control_url),
        games: games.clone(),
        exit_secret: Some(exit_secret),
        control_api_key: Some(exit_api_key.clone()),
    })
    .await
    .unwrap();
    let exit_peer = Peer {
        id: EXIT_ID,
        name: "exit".into(),
        address: exit.local_addr(),
        roles: vec![Role::Exit],
    };
    let entry = Relay::start(RelayOptions {
        config: relay_config(
            ENTRY_ID,
            vec![Role::Entry],
            issuer,
            vec![exit_peer],
            &control_url,
        ),
        games: games.clone(),
        exit_secret: None,
        control_api_key: Some(entry_api_key.clone()),
    })
    .await
    .unwrap();

    let control = Control::start(ControlOptions {
        config: ControlConfig {
            listen: control_listen,
            admin_listen: Some(SocketAddr::from((LOCALHOST, 0))),
            issuer_seed_file: "unused".into(),
            games_dir: None,
            client_ip_header: None,
            sessions: SessionLimits {
                min_renew_interval_secs: 0,
                ..Default::default()
            },
            relays: vec![
                RelayEntry {
                    id: EXIT_ID,
                    name: "exit".into(),
                    address: exit.local_addr(),
                    roles: vec![ControlRole::Exit],
                    regions: vec![REGION.into()],
                    public_key: Some(exit_public),
                    api_key_sha256: Some(api_key_hash(&exit_api_key)),
                    provider: None,
                    location: None,
                },
                RelayEntry {
                    id: ENTRY_ID,
                    name: "entry".into(),
                    address: entry.local_addr(),
                    roles: vec![ControlRole::Entry],
                    regions: vec![],
                    public_key: None,
                    api_key_sha256: Some(api_key_hash(&entry_api_key)),
                    provider: None,
                    location: None,
                },
            ],
        },
        games,
        issuer_seed,
    })
    .await
    .unwrap();
    let api = format!("http://{}", control.api_addr());

    // A session for a routable game, through the entry the answer lists.
    let client = WireGuardKeys::generate().unwrap();
    let request = json!({ "client_public_key": client.public_base64(), "game": "test-game", "region": REGION });
    let (status, grant) = http("POST", format!("{api}/v1/sessions"), Some(request), None).await;
    assert_eq!(status, 201, "{grant}");
    assert_eq!(grant["exit"]["id"], EXIT_ID);
    assert_eq!(grant["entries"][0]["id"], ENTRY_ID);
    let token = base64::engine::general_purpose::STANDARD
        .decode(grant["token"].as_str().unwrap())
        .unwrap();
    let entry_addr: SocketAddr = grant["entries"][0]["address"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let exit_key =
        keys::public_key_from_base64(grant["exit"]["public_key"].as_str().unwrap()).unwrap();
    let mut session = ClientSession::connect(entry_addr, &token, &client, exit_key, TIMEOUT)
        .await
        .unwrap();
    session
        .send_udp(40_000, echo, b"through the control plane")
        .await
        .unwrap();
    assert_eq!(
        session.recv_udp(TIMEOUT).await.unwrap().payload,
        b"through the control plane"
    );

    // Renewal: a new token for the same session, accepted without a new handshake.
    let renew = json!({ "session_id": grant["session_id"], "renew_secret": grant["renew_secret"] });
    let (status, renewed) = http(
        "POST",
        format!("{api}/v1/sessions/renew"),
        Some(renew),
        None,
    )
    .await;
    assert_eq!(status, 200, "{renewed}");
    assert_eq!(renewed["session_id"], grant["session_id"]);
    let token = base64::engine::general_purpose::STANDARD
        .decode(renewed["token"].as_str().unwrap())
        .unwrap();
    session.hello(&token, TIMEOUT).await.unwrap();
    session
        .send_udp(40_000, echo, b"after renewal")
        .await
        .unwrap();
    assert_eq!(
        session.recv_udp(TIMEOUT).await.unwrap().payload,
        b"after renewal"
    );
    assert_eq!(exit.metrics().exit_sessions.load(Ordering::Relaxed), 1);

    // Refusals carry a stable code.
    let untested = json!({ "client_public_key": WireGuardKeys::generate().unwrap().public_base64(), "game": "untested-game", "region": REGION });
    let (status, refusal) = http("POST", format!("{api}/v1/sessions"), Some(untested), None).await;
    assert_eq!(
        (status, refusal["error"].as_str()),
        (403, Some("game_not_routable"))
    );
    let (status, refusal) = http("POST", format!("{api}/v1/sessions"), None, None).await;
    assert_eq!(
        (status, refusal["error"].as_str()),
        (400, Some("invalid_request"))
    );
    let report = json!({ "relay_id": ENTRY_ID, "samples": [] });
    let (status, _) = http(
        "POST",
        format!("{api}/v1/relay-latency"),
        Some(report.clone()),
        None,
    )
    .await;
    assert_eq!(status, 401);
    let (status, _) = http(
        "POST",
        format!("{api}/v1/relay-latency"),
        Some(report),
        Some("wrong".into()),
    )
    .await;
    assert_eq!(status, 401);

    // The relay list, and the entry's measurement of the exit.
    let (_, relays) = http("GET", format!("{api}/v1/relays"), None, None).await;
    assert_eq!(relays["relays"].as_array().unwrap().len(), 2);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, latency) = http("GET", format!("{api}/v1/latency"), None, None).await;
        let rows = latency["latency"].as_array().unwrap();
        let relay_row = rows
            .iter()
            .find(|r| r["relay_id"] == ENTRY_ID && r["peer_id"] == EXIT_ID);
        // The exit's measurement of its game region.
        let region_row = latency["regions"].as_array().unwrap().iter().find(|r| {
            r["relay_id"] == EXIT_ID
                && r["game"] == "test-game"
                && r["region"] == REGION
                && r["rtt_ms"].is_number()
        });
        if let (Some(relay_row), Some(region_row)) = (relay_row, region_row) {
            assert!(relay_row["rtt_ms"].as_f64().unwrap() < 50.0, "{relay_row}");
            assert!(
                region_row["rtt_ms"].as_f64().unwrap() < 50.0,
                "{region_row}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no latency report arrived: {latency}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let admin = format!("http://{}", control.admin_addr().unwrap());
    let metrics = tokio::task::spawn_blocking(move || {
        ureq::get(&format!("{admin}/metrics"))
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        metrics.contains("pingpath_control_sessions_issued_total 1\n"),
        "{metrics}"
    );
    assert!(
        metrics.contains("pingpath_control_sessions_renewed_total 1\n"),
        "{metrics}"
    );
    assert!(
        metrics.contains("pingpath_control_refusals_total{reason=\"game_not_routable\"} 1\n"),
        "{metrics}"
    );
    control.shutdown();
}
