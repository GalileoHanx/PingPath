//! Shared setup for the relay's end-to-end tests: an exit relay and an entry
//! relay on loopback, UDP echo servers standing in for game servers, and a
//! token issuer.

#![allow(dead_code)] // each test binary uses a different subset

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use pingpath_games::{AntiCheat, GameDefinition, PortRange, Ports, Region, SupportStatus};
use pingpath_relay::config::{Config, Limits, Peer, ProbeConfig, Role};
use pingpath_relay::{Relay, RelayOptions};
use pingpath_tunnel::client::{ClientError, ClientSession};
use pingpath_tunnel::keys::{self, WireGuardKeys};
use pingpath_tunnel::token::{Claims, Issuer, now_unix};
use tokio::net::UdpSocket;

pub const EXIT_ID: u32 = 1;
pub const ENTRY_ID: u32 = 2;
/// The one region the test exit serves.
pub const REGION: &str = "here";
pub const TIMEOUT: Duration = Duration::from_secs(5);
pub const LOCALHOST: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// A UDP server that sends every datagram back.
pub async fn echo_server() -> SocketAddrV4 {
    let socket = UdpSocket::bind((LOCALHOST, 0)).await.unwrap();
    let addr = match socket.local_addr().unwrap() {
        SocketAddr::V4(a) => a,
        SocketAddr::V6(_) => unreachable!(),
    };
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = socket.recv_from(&mut buf).await {
            let _ = socket.send_to(&buf[..n], from).await;
        }
    });
    addr
}

/// A game whose only servers are the given loopback ports. Built directly,
/// because the definition validator (rightly) forbids loopback addresses.
pub fn game(id: &str, status: SupportStatus, ports: &[u16]) -> GameDefinition {
    GameDefinition {
        id: id.into(),
        name: id.into(),
        status,
        status_reason: None,
        report: None,
        anti_cheat: vec![AntiCheat::None],
        ports: Ports {
            udp: ports
                .iter()
                .map(|&p| PortRange { first: p, last: p })
                .collect(),
            tcp: vec![],
        },
        sdr_app_id: None,
        regions: vec![Region {
            id: REGION.into(),
            name: "This machine".into(),
            server_ranges: vec!["127.0.0.0/8".parse().unwrap()],
            ping_targets: vec![],
            sdr_pops: vec![],
            location: None,
        }],
        notes: None,
    }
}

pub fn config(
    id: u32,
    roles: Vec<Role>,
    issuer: &Issuer,
    relays: Vec<Peer>,
    limits: Limits,
) -> Config {
    Config {
        id,
        name: format!("relay-{id}"),
        listen: SocketAddr::from((LOCALHOST, 0)),
        http: Some(SocketAddr::from((LOCALHOST, 0))),
        roles,
        private_key_file: None,
        regions: vec![REGION.into()],
        issuer_public_keys: vec![keys::encode(&issuer.public_key())],
        games_dir: None,
        limits,
        relays,
        probe: ProbeConfig {
            interval_secs: 0,
            probes_per_round: 0,
        },
        control: None,
    }
}

pub struct Setup {
    pub issuer: Issuer,
    pub exit: Relay,
    pub entry: Relay,
    pub echo: SocketAddrV4,
    /// A second server on loopback that the game does not list.
    pub forbidden: SocketAddrV4,
}

pub async fn setup(limits: Limits) -> Setup {
    let issuer = Issuer::from_seed(&keys::random_32().unwrap()).unwrap();
    let echo = echo_server().await;
    let forbidden = echo_server().await;
    let games = vec![
        game("test-game", SupportStatus::Supported, &[echo.port()]),
        game("untested-game", SupportStatus::Untested, &[echo.port()]),
    ];
    // The exit lists the entry as a peer, as in production (it trusts HELLOs
    // the entry forwards), so the entry's address is picked first.
    let entry_addr = std::net::UdpSocket::bind((LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap();
    let entry_peer = Peer {
        id: ENTRY_ID,
        name: "entry".into(),
        address: entry_addr,
        roles: vec![Role::Entry],
    };
    let exit = Relay::start(RelayOptions {
        config: config(
            EXIT_ID,
            vec![Role::Exit],
            &issuer,
            vec![entry_peer],
            limits.clone(),
        ),
        games: games.clone(),
        exit_secret: Some(keys::random_32().unwrap()),
        control_api_key: None,
    })
    .await
    .unwrap();
    let exit_peer = Peer {
        id: EXIT_ID,
        name: "exit".into(),
        address: exit.local_addr(),
        roles: vec![Role::Exit],
    };
    let mut entry_config = config(
        ENTRY_ID,
        vec![Role::Entry],
        &issuer,
        vec![exit_peer],
        limits,
    );
    entry_config.listen = entry_addr;
    let entry = Relay::start(RelayOptions {
        config: entry_config,
        games,
        exit_secret: None,
        control_api_key: None,
    })
    .await
    .unwrap();
    Setup {
        issuer,
        exit,
        entry,
        echo,
        forbidden,
    }
}

pub fn claims(client: &WireGuardKeys, game: &str) -> Claims {
    let now = now_unix();
    Claims {
        session_id: keys::random_32().unwrap()[..8].try_into().unwrap(),
        client_key: *client.public.as_bytes(),
        exit_id: EXIT_ID,
        issued_at: now - 1,
        expires_at: now + 900,
        game: game.into(),
        region: REGION.into(),
    }
}

impl Setup {
    pub async fn connect_via(
        &self,
        relay: SocketAddr,
        game: &str,
    ) -> Result<(ClientSession, WireGuardKeys), ClientError> {
        let client = WireGuardKeys::generate().unwrap();
        let token = self.issuer.sign(&claims(&client, game)).unwrap();
        let session = ClientSession::connect(
            relay,
            &token,
            &client,
            self.exit.exit_public_key().unwrap(),
            TIMEOUT,
        )
        .await?;
        Ok((session, client))
    }

    pub async fn connect(&self) -> ClientSession {
        self.connect_via(self.entry.local_addr(), "test-game")
            .await
            .unwrap()
            .0
    }
}

pub async fn echo_round_trip(
    session: &mut ClientSession,
    server: SocketAddrV4,
    payload: &[u8],
) -> Result<Duration, ClientError> {
    let start = Instant::now();
    session.send_udp(40_000, server, payload).await?;
    let reply = session.recv_udp(TIMEOUT).await?;
    assert_eq!(
        reply.payload, payload,
        "payload must come back unchanged (rule 2)"
    );
    assert_eq!(reply.src, server);
    assert_eq!(reply.dst.port(), 40_000);
    Ok(start.elapsed())
}

pub fn flows(relay: &Relay) -> i64 {
    relay.metrics().flows.load(Ordering::Relaxed)
}

pub fn exit_sessions(relay: &Relay) -> i64 {
    relay.metrics().exit_sessions.load(Ordering::Relaxed)
}
