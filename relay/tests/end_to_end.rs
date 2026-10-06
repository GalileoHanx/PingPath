//! End to end on one machine: a client session through an entry relay and
//! an exit relay to a UDP echo server standing in for a game server. These
//! are PLAN.md Phase 2's done-when criteria:
//!
//! - UDP goes client → entry → exit → echo server and back, unchanged;
//! - relay-added latency stays under 1 ms at the 99th percentile
//!   (checked in release builds; see `added_latency_is_under_a_millisecond`);
//! - the exit refuses destinations outside the game's allowlist;
//! - relays refuse sessions without a valid token;
//! - duplicated packets (Phase 4 multipath) are delivered once, each way.

mod common;

use std::net::{SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use pingpath_games::SupportStatus;
use pingpath_relay::config::{Limits, Peer, ProbeConfig, Role};
use pingpath_relay::metrics::DropReason;
use pingpath_relay::{Relay, RelayOptions};
use pingpath_tunnel::client::{ClientError, ClientSession};
use pingpath_tunnel::frame::{self, Frame, HelloStatus};
use pingpath_tunnel::keys::{self, WireGuardKeys};
use pingpath_tunnel::token::{Claims, Issuer, now_unix};
use tokio::net::UdpSocket;

#[tokio::test]
async fn udp_goes_through_entry_and_exit_unchanged() {
    let s = setup(Limits::default()).await;
    let mut session = s.connect().await;
    for payload in [
        &b"ping"[..],
        &[0u8; 1200][..],
        &(0..=255).collect::<Vec<u8>>()[..],
    ] {
        echo_round_trip(&mut session, s.echo, payload)
            .await
            .unwrap();
    }
    assert_eq!(flows(&s.exit), 1);
    assert_eq!(
        s.entry
            .metrics()
            .entry_sessions
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(exit_sessions(&s.exit), 1);
}

#[tokio::test]
async fn a_client_can_go_straight_to_the_exit() {
    let s = setup(Limits::default()).await;
    let (mut session, _) = s
        .connect_via(s.exit.local_addr(), "test-game")
        .await
        .unwrap();
    echo_round_trip(&mut session, s.echo, b"direct")
        .await
        .unwrap();
    assert_eq!(
        s.entry
            .metrics()
            .frames_received
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[tokio::test]
async fn the_exit_refuses_destinations_outside_the_allowlist() {
    let s = setup(Limits::default()).await;
    let mut session = s.connect().await;

    // Watch the forbidden server directly: nothing may reach it.
    let watcher = UdpSocket::bind((LOCALHOST, 0)).await.unwrap();
    let watched = SocketAddrV4::new(LOCALHOST, watcher.local_addr().unwrap().port());
    for target in [s.forbidden, watched] {
        session
            .send_udp(40_001, target, b"should not arrive")
            .await
            .unwrap();
        assert!(matches!(
            session.recv_udp(Duration::from_millis(500)).await,
            Err(ClientError::Timeout(_))
        ));
    }
    let mut buf = [0u8; 64];
    assert!(
        tokio::time::timeout(Duration::from_millis(200), watcher.recv_from(&mut buf))
            .await
            .is_err()
    );
    assert_eq!(s.exit.metrics().drops(DropReason::NotAllowlisted), 2);

    // The session still works for the game's real server.
    echo_round_trip(&mut session, s.echo, b"still fine")
        .await
        .unwrap();
}

#[tokio::test]
async fn relays_refuse_sessions_without_a_valid_token() {
    let s = setup(Limits::default()).await;
    let client = WireGuardKeys::generate().unwrap();
    let exit_key = s.exit.exit_public_key().unwrap();
    let entry = s.entry.local_addr();
    let refused = |result: Result<ClientSession, ClientError>| match result {
        Err(ClientError::Refused(status)) => status,
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("expected a refusal, got a session"),
    };

    // Tokens that don't verify get no answer at all, so a relay never sends
    // anything to a spoofed address in reply to garbage.
    let silent = |result: Result<ClientSession, ClientError>| {
        assert!(
            matches!(result, Err(ClientError::Timeout(_))),
            "expected silence, got {:?}",
            result.err()
        );
    };
    let short = Duration::from_millis(1200);

    // Signed by someone else.
    let forger = Issuer::from_seed(&keys::random_32().unwrap()).unwrap();
    let forged = forger.sign(&claims(&client, "test-game")).unwrap();
    silent(ClientSession::connect(entry, &forged, &client, exit_key, short).await);

    // Expired.
    let mut old = claims(&client, "test-game");
    old.issued_at = now_unix() - 3600;
    old.expires_at = now_unix() - 600;
    let expired = s.issuer.sign(&old).unwrap();
    assert_eq!(
        refused(ClientSession::connect(entry, &expired, &client, exit_key, TIMEOUT).await),
        HelloStatus::Expired
    );

    // For an exit the entry doesn't know.
    let mut elsewhere = claims(&client, "test-game");
    elsewhere.exit_id = 99;
    let elsewhere = s.issuer.sign(&elsewhere).unwrap();
    assert_eq!(
        refused(ClientSession::connect(entry, &elsewhere, &client, exit_key, TIMEOUT).await),
        HelloStatus::WrongExit
    );

    // For a game that isn't routable (rule 7).
    assert!(matches!(
        s.connect_via(entry, "untested-game").await,
        Err(ClientError::Refused(HelloStatus::GameNotRoutable))
    ));

    // Bit-flipped: tampering breaks the signature.
    let mut tampered = s.issuer.sign(&claims(&client, "test-game")).unwrap();
    tampered[20] ^= 0x40;
    silent(ClientSession::connect(entry, &tampered, &client, exit_key, short).await);
    assert!(s.entry.metrics().drops(DropReason::BadToken) >= 2);

    // DATA with no HELLO at all is dropped by the entry and the exit.
    let raw = UdpSocket::bind((LOCALHOST, 0)).await.unwrap();
    for relay in [entry, s.exit.local_addr()] {
        raw.send_to(&frame::data(&[7; 8], &[4, 0, 0, 0, 1, 2, 3, 4]), relay)
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(s.entry.metrics().drops(DropReason::UnknownSession) >= 1);
    assert!(s.exit.metrics().drops(DropReason::UnknownSession) >= 1);
    // Every attempt was refused before the exit created a session.
    assert_eq!(exit_sessions(&s.exit), 0);
}

#[tokio::test]
async fn frames_from_another_address_cannot_use_a_session() {
    let s = setup(Limits::default()).await;
    let mut session = s.connect().await;
    let stranger = UdpSocket::bind((LOCALHOST, 0)).await.unwrap();
    stranger
        .send_to(
            &frame::data(&session.session_id(), &[4; 48]),
            s.entry.local_addr(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(s.entry.metrics().drops(DropReason::WrongSource), 1);
    echo_round_trip(&mut session, s.echo, b"unaffected")
        .await
        .unwrap();
}

#[tokio::test]
async fn renewing_the_token_keeps_the_session_and_its_flows() {
    let s = setup(Limits::default()).await;
    let client = WireGuardKeys::generate().unwrap();
    let first = claims(&client, "test-game");
    let token = s.issuer.sign(&first).unwrap();
    let mut session = ClientSession::connect(
        s.entry.local_addr(),
        &token,
        &client,
        s.exit.exit_public_key().unwrap(),
        TIMEOUT,
    )
    .await
    .unwrap();
    echo_round_trip(&mut session, s.echo, b"before")
        .await
        .unwrap();

    let renewed = s
        .issuer
        .sign(&Claims {
            expires_at: first.expires_at + 900,
            issued_at: now_unix(),
            ..first.clone()
        })
        .unwrap();
    session.hello(&renewed, TIMEOUT).await.unwrap();
    echo_round_trip(&mut session, s.echo, b"after")
        .await
        .unwrap();
    assert_eq!(flows(&s.exit), 1, "same flow, same source port");
    assert_eq!(
        s.exit
            .metrics()
            .sessions_started
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );

    // A token reusing the session ID with another client key is refused.
    let impostor = WireGuardKeys::generate().unwrap();
    let hijack = s
        .issuer
        .sign(&Claims {
            client_key: *impostor.public.as_bytes(),
            ..first
        })
        .unwrap();
    assert!(matches!(
        session.hello(&hijack, TIMEOUT).await,
        Err(ClientError::Refused(HelloStatus::BadToken))
    ));
}

#[tokio::test]
async fn sessions_are_rate_limited() {
    let limits = Limits {
        packets_per_second: 20,
        ..Limits::default()
    };
    let s = setup(limits).await;
    let mut session = s.connect().await;
    for i in 0..100u16 {
        session
            .send_udp(40_000 + i % 4, s.echo, b"flood")
            .await
            .unwrap();
    }
    let mut echoed = 0;
    while session.recv_udp(Duration::from_millis(300)).await.is_ok() {
        echoed += 1;
    }
    assert!(
        echoed < 60,
        "{echoed} of 100 packets got through a 20 packet/s limit"
    );
    let limited = s.entry.metrics().drops(DropReason::RateLimited)
        + s.exit.metrics().drops(DropReason::RateLimited);
    assert!(limited > 0);
}

/// Done-when: relay processing adds under 1 ms at the 99th percentile.
/// Compares round trips to the echo server through entry and exit with
/// round trips sent to it directly, on the same machine. The difference
/// also includes the client's own encryption, so it overstates the relays'
/// share. Debug builds are far slower, so the 1 ms bound applies to
/// release builds (CI runs this test with `--release`).
#[tokio::test]
async fn added_latency_is_under_a_millisecond() {
    const ROUNDS: usize = 2_000;
    let s = setup(Limits {
        packets_per_second: 100_000,
        bits_per_second: 1_000_000_000,
        ..Limits::default()
    })
    .await;
    let mut session = s.connect().await;
    let payload = [0x5a; 100];

    let direct = UdpSocket::bind((LOCALHOST, 0)).await.unwrap();
    direct.connect(s.echo).await.unwrap();
    let mut buf = [0u8; 256];
    let mut direct_rtts = Vec::with_capacity(ROUNDS);
    let mut relay_rtts = Vec::with_capacity(ROUNDS);
    for _ in 0..50 {
        echo_round_trip(&mut session, s.echo, &payload)
            .await
            .unwrap(); // warm up
    }
    for _ in 0..ROUNDS {
        let start = Instant::now();
        direct.send(&payload).await.unwrap();
        direct.recv(&mut buf).await.unwrap();
        direct_rtts.push(start.elapsed());
        relay_rtts.push(
            echo_round_trip(&mut session, s.echo, &payload)
                .await
                .unwrap(),
        );
    }
    let p99 = |v: &mut Vec<Duration>| {
        v.sort();
        v[v.len() * 99 / 100]
    };
    let (direct_p99, relay_p99) = (p99(&mut direct_rtts), p99(&mut relay_rtts));
    let added = relay_p99.saturating_sub(direct_p99);
    eprintln!("p99 direct {direct_p99:?}, through entry and exit {relay_p99:?}, added {added:?}");
    let bound = if cfg!(debug_assertions) {
        Duration::from_millis(25)
    } else {
        Duration::from_millis(1)
    };
    assert!(
        added < bound,
        "relays added {added:?} at p99 (bound {bound:?})"
    );
}

#[tokio::test]
async fn health_and_metrics_are_served() {
    let s = setup(Limits::default()).await;
    let mut session = s.connect().await;
    echo_round_trip(&mut session, s.echo, b"x").await.unwrap();
    let base = format!("http://{}", s.exit.http_addr().unwrap());
    let get = |url: String| {
        tokio::task::spawn_blocking(move || {
            let mut response = ureq::get(&url).call().unwrap();
            response.body_mut().read_to_string().unwrap()
        })
    };
    let health: serde_json::Value =
        serde_json::from_str(&get(format!("{base}/healthz")).await.unwrap()).unwrap();
    assert_eq!(health["status"], "ok");
    assert_eq!(health["id"], EXIT_ID);
    assert_eq!(health["exit_sessions"], 1);
    let metrics = get(format!("{base}/metrics")).await.unwrap();
    assert!(metrics.contains("pingpath_relay_flows 1"), "{metrics}");
    assert!(
        metrics.contains("pingpath_relay_drops_total{reason=\"not_allowlisted\"} 0"),
        "{metrics}"
    );
}

#[tokio::test]
async fn relays_probe_each_other() {
    let issuer = Issuer::from_seed(&keys::random_32().unwrap()).unwrap();
    let exit = Relay::start(RelayOptions {
        config: config(
            EXIT_ID,
            vec![Role::Exit],
            &issuer,
            vec![],
            Limits::default(),
        ),
        games: vec![],
        exit_secret: Some(keys::random_32().unwrap()),
        control_api_key: None,
    })
    .await
    .unwrap();
    let mut entry_config = config(
        ENTRY_ID,
        vec![Role::Entry],
        &issuer,
        vec![Peer {
            id: EXIT_ID,
            name: "exit".into(),
            address: exit.local_addr(),
            roles: vec![Role::Exit],
        }],
        Limits::default(),
    );
    entry_config.probe = ProbeConfig {
        interval_secs: 1,
        probes_per_round: 3,
    };
    let entry = Relay::start(RelayOptions {
        config: entry_config,
        games: vec![],
        exit_secret: None,
        control_api_key: None,
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let peers = entry.metrics().peer_latency();
    assert_eq!(peers.len(), 1, "{peers:?}");
    assert!(peers[0].rtt_ms.is_some(), "{peers:?}");
    assert_eq!(peers[0].loss_pct, 0.0, "{peers:?}");
}

#[tokio::test]
async fn the_exit_refuses_a_region_it_does_not_serve() {
    let s = setup(Limits::default()).await;
    let client = WireGuardKeys::generate().unwrap();
    let mut elsewhere = claims(&client, "test-game");
    elsewhere.region = "eu-north".into();
    let token = s.issuer.sign(&elsewhere).unwrap();
    let result = ClientSession::connect(
        s.entry.local_addr(),
        &token,
        &client,
        s.exit.exit_public_key().unwrap(),
        TIMEOUT,
    )
    .await;
    assert!(
        matches!(result, Err(ClientError::Refused(HelloStatus::WrongRegion))),
        "{:?}",
        result.err()
    );
    assert_eq!(s.exit.metrics().drops(DropReason::WrongRegion), 1);
}

#[tokio::test]
async fn the_in_tunnel_echo_answers_without_touching_game_servers() {
    let s = setup(Limits::default()).await;
    let mut session = s.connect().await;
    for _ in 0..5 {
        session.echo(TIMEOUT).await.unwrap();
    }
    assert_eq!(flows(&s.exit), 0, "the echo never opens a flow");
}

#[tokio::test]
async fn the_return_path_moves_only_after_a_handshake_from_the_new_address() {
    let s = setup(Limits::default()).await;
    let (mut session, _) = s
        .connect_via(s.exit.local_addr(), "test-game")
        .await
        .unwrap();
    echo_round_trip(&mut session, s.echo, b"before")
        .await
        .unwrap();

    // Same keys from a new address: the data authenticates, but that doesn't
    // prove the address belongs to the client (it could be spoofed to aim
    // replies at someone else), so replies keep going to the confirmed one.
    session.rebind().await.unwrap();
    session
        .send_udp(40_000, s.echo, b"reply goes to the old address")
        .await
        .unwrap();
    assert!(matches!(
        session.recv_udp(Duration::from_millis(700)).await,
        Err(ClientError::Timeout(_))
    ));

    // A handshake from the new address proves it receives traffic.
    session.handshake(TIMEOUT).await.unwrap();
    echo_round_trip(&mut session, s.echo, b"after")
        .await
        .unwrap();
}

#[tokio::test]
async fn through_an_entry_a_new_address_must_say_hello_again() {
    let s = setup(Limits::default()).await;
    let client = WireGuardKeys::generate().unwrap();
    let token = s.issuer.sign(&claims(&client, "test-game")).unwrap();
    let exit_key = s.exit.exit_public_key().unwrap();
    let mut session =
        ClientSession::connect(s.entry.local_addr(), &token, &client, exit_key, TIMEOUT)
            .await
            .unwrap();
    echo_round_trip(&mut session, s.echo, b"before")
        .await
        .unwrap();

    session.rebind().await.unwrap();
    session.send_udp(40_000, s.echo, b"dropped").await.unwrap();
    assert!(matches!(
        session.recv_udp(Duration::from_millis(500)).await,
        Err(ClientError::Timeout(_))
    ));
    assert!(s.entry.metrics().drops(DropReason::WrongSource) >= 1);

    // HELLO again: the entry checks the new address with a cookie first.
    session.hello(&token, TIMEOUT).await.unwrap();
    echo_round_trip(&mut session, s.echo, b"after")
        .await
        .unwrap();
}

#[tokio::test]
async fn sessions_have_a_flow_limit() {
    let s = setup(Limits {
        max_flows_per_session: 2,
        ..Limits::default()
    })
    .await;
    let mut session = s.connect().await;
    for port in [41_000, 41_001, 41_002] {
        session.send_udp(port, s.echo, b"x").await.unwrap();
    }
    let mut echoed = 0;
    while session.recv_udp(Duration::from_millis(300)).await.is_ok() {
        echoed += 1;
    }
    assert_eq!(echoed, 2);
    assert_eq!(s.exit.metrics().drops(DropReason::FlowLimit), 1);
}

#[tokio::test]
async fn traffic_to_one_server_is_capped_across_sessions() {
    let s = setup(Limits {
        destination_packets_per_second: 10,
        ..Limits::default()
    })
    .await;
    let mut a = s.connect().await;
    let mut b = s.connect().await;
    for _ in 0..15 {
        a.send_udp(42_000, s.echo, b"a").await.unwrap();
        b.send_udp(42_000, s.echo, b"b").await.unwrap();
    }
    let mut echoed = 0;
    for session in [&mut a, &mut b] {
        while session.recv_udp(Duration::from_millis(300)).await.is_ok() {
            echoed += 1;
        }
    }
    assert!(
        echoed <= 12,
        "{echoed} of 30 packets reached a server capped at 10 packets/s"
    );
    assert!(s.exit.metrics().drops(DropReason::DestinationLimited) >= 18);
}

#[tokio::test]
async fn a_client_key_serves_one_session_at_a_time() {
    let s = setup(Limits::default()).await;
    let client = WireGuardKeys::generate().unwrap();
    let exit_key = s.exit.exit_public_key().unwrap();
    let first = s.issuer.sign(&claims(&client, "test-game")).unwrap();
    let _session = ClientSession::connect(s.exit.local_addr(), &first, &client, exit_key, TIMEOUT)
        .await
        .unwrap();
    // A new session ID with the same key: refused, so a handshake recorded in
    // one session can't be replayed into another.
    let second = s.issuer.sign(&claims(&client, "test-game")).unwrap();
    let result =
        ClientSession::connect(s.exit.local_addr(), &second, &client, exit_key, TIMEOUT).await;
    assert!(
        matches!(result, Err(ClientError::Refused(HelloStatus::BadToken))),
        "{:?}",
        result.err()
    );
}

#[tokio::test]
async fn a_hello_without_a_handshake_expires() {
    let s = setup(Limits {
        pending_session_secs: 1,
        ..Limits::default()
    })
    .await;
    let client = WireGuardKeys::generate().unwrap();
    let grant = claims(&client, "test-game");
    let token = s.issuer.sign(&grant).unwrap();
    let raw = UdpSocket::bind((LOCALHOST, 0)).await.unwrap();
    raw.send_to(
        &frame::hello(&grant.session_id, &token, None),
        s.exit.local_addr(),
    )
    .await
    .unwrap();
    let mut buf = [0u8; 64];
    let (n, _) = tokio::time::timeout(TIMEOUT, raw.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        Frame::parse(&buf[..n]),
        Ok(Frame::HelloAck {
            status: HelloStatus::OkNewSession,
            ..
        })
    ));
    assert_eq!(exit_sessions(&s.exit), 1);
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(
        exit_sessions(&s.exit),
        0,
        "a session nobody completed is dropped"
    );
}

/// A UDP hop that sends every datagram on twice, both ways: what the exit
/// and the client see when Phase 4 sends each packet over two paths.
async fn duplicating_hop(target: SocketAddr) -> SocketAddr {
    let socket = Arc::new(UdpSocket::bind((LOCALHOST, 0)).await.unwrap());
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        let mut client = None;
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = socket.recv_from(&mut buf).await {
            let to = if from == target {
                match client {
                    Some(client) => client,
                    None => continue,
                }
            } else {
                client = Some(from);
                target
            };
            for _ in 0..2 {
                let _ = socket.send_to(&buf[..n], to).await;
            }
        }
    });
    addr
}

#[tokio::test]
async fn duplicated_packets_are_delivered_once_each_way() {
    let s = setup(Limits::default()).await;
    let hop = duplicating_hop(s.exit.local_addr()).await;
    let (mut session, _) = s.connect_via(hop, "test-game").await.unwrap();
    for payload in [&b"first"[..], b"second", b"third"] {
        // If the exit forwarded both copies, the echo server would answer
        // twice; if the client accepted both copies of the answer, it would
        // see it twice. Either way a second datagram would arrive.
        session.send_udp(40_000, s.echo, payload).await.unwrap();
        assert_eq!(session.recv_udp(TIMEOUT).await.unwrap().payload, payload);
        assert!(
            matches!(
                session.recv_udp(Duration::from_millis(300)).await,
                Err(ClientError::Timeout(_))
            ),
            "a duplicate got through"
        );
    }
    assert!(s.exit.metrics().drops(DropReason::Duplicate) >= 3);
    assert_eq!(flows(&s.exit), 1);
}

#[tokio::test]
async fn proven_and_forwarded_hellos_are_not_throttled_like_strangers() {
    // A dead entry moves all its players to another entry at once: their
    // HELLOs carry valid cookies, and that entry forwards them all to the
    // exit from its own address. Neither may be cut to the per-network budget.
    let s = setup(Limits {
        handshakes_per_second: 2,
        ..Limits::default()
    })
    .await;
    let client = WireGuardKeys::generate().unwrap();
    let grant = claims(&client, "test-game");
    let token = s.issuer.sign(&grant).unwrap();
    let socket = UdpSocket::bind((LOCALHOST, 0)).await.unwrap();
    socket.connect(s.entry.local_addr()).await.unwrap();
    socket
        .send(&frame::hello(&grant.session_id, &token, None))
        .await
        .unwrap();
    let mut buf = [0u8; 256];
    let n = tokio::time::timeout(TIMEOUT, socket.recv(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let Ok(Frame::HelloAck {
        cookie: Some(cookie),
        ..
    }) = Frame::parse(&buf[..n])
    else {
        panic!("expected a cookie");
    };
    for _ in 0..10 {
        socket
            .send(&frame::hello(&grant.session_id, &token, Some(&cookie)))
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.entry.metrics().drops(DropReason::HelloFlood), 0);
    assert_eq!(s.exit.metrics().drops(DropReason::HelloFlood), 0);
    assert_eq!(exit_sessions(&s.exit), 1);

    // A token the entry already verified costs a lookup, so it gets the
    // larger budget even without a cookie (a player fetching a standby
    // entry's cookie ahead of a failover, behind a carrier NAT).
    for _ in 0..10 {
        socket
            .send(&frame::hello(&grant.session_id, &token, None))
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(s.entry.metrics().drops(DropReason::HelloFlood), 0);

    // Strangers without a cookie, with tokens it hasn't seen, still get the
    // small budget.
    for _ in 0..10 {
        let stranger = claims(&WireGuardKeys::generate().unwrap(), "test-game");
        let token = s.issuer.sign(&stranger).unwrap();
        socket
            .send(&frame::hello(&stranger.session_id, &token, None))
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(s.entry.metrics().drops(DropReason::HelloFlood) >= 7);
}

#[tokio::test]
async fn probes_do_not_use_up_the_hello_budget() {
    // Players measuring behind one carrier NAT send many PROBEs; the HELLOs
    // of those who then connect must still get through.
    let s = setup(Limits {
        handshakes_per_second: 2,
        probes_per_second: 5,
        ..Limits::default()
    })
    .await;
    let socket = UdpSocket::bind((LOCALHOST, 0)).await.unwrap();
    socket.connect(s.entry.local_addr()).await.unwrap();
    for nonce in 0..20 {
        socket.send(&frame::probe(nonce, 0)).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(s.entry.metrics().drops(DropReason::ProbeFlood) >= 10);

    let grant = claims(&WireGuardKeys::generate().unwrap(), "test-game");
    let token = s.issuer.sign(&grant).unwrap();
    socket
        .send(&frame::hello(&grant.session_id, &token, None))
        .await
        .unwrap();
    let mut buf = [0u8; 256];
    let status = loop {
        let n = tokio::time::timeout(TIMEOUT, socket.recv(&mut buf))
            .await
            .unwrap()
            .unwrap();
        match Frame::parse(&buf[..n]) {
            Ok(Frame::ProbeReply { .. }) => continue,
            Ok(Frame::HelloAck { status, .. }) => break status,
            other => panic!("unexpected {other:?}"),
        }
    };
    assert_eq!(status, HelloStatus::Verify);
    assert_eq!(s.entry.metrics().drops(DropReason::HelloFlood), 0);
}

#[tokio::test]
async fn a_relay_that_is_entry_and_exit_forwards_hellos_to_another_exit() {
    // The usual layout: every relay is both entry and exit, so a player's
    // path can enter at one and leave at another. The exit must accept the
    // HELLOs the other forwards, at connect and at renewal.
    let issuer = Issuer::from_seed(&keys::random_32().unwrap()).unwrap();
    let echo = echo_server().await;
    let games = vec![game("test-game", SupportStatus::Supported, &[echo.port()])];
    let both = vec![Role::Entry, Role::Exit];
    let entry_addr = std::net::UdpSocket::bind((LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap();
    let exit = Relay::start(RelayOptions {
        config: config(
            EXIT_ID,
            both.clone(),
            &issuer,
            vec![Peer {
                id: ENTRY_ID,
                name: "other".into(),
                address: entry_addr,
                roles: both.clone(),
            }],
            Limits::default(),
        ),
        games: games.clone(),
        exit_secret: Some(keys::random_32().unwrap()),
        control_api_key: None,
    })
    .await
    .unwrap();
    let mut entry_config = config(
        ENTRY_ID,
        both.clone(),
        &issuer,
        vec![Peer {
            id: EXIT_ID,
            name: "exit".into(),
            address: exit.local_addr(),
            roles: both,
        }],
        Limits::default(),
    );
    entry_config.listen = entry_addr;
    let entry = Relay::start(RelayOptions {
        config: entry_config,
        games,
        exit_secret: Some(keys::random_32().unwrap()),
        control_api_key: None,
    })
    .await
    .unwrap();

    let client = WireGuardKeys::generate().unwrap();
    let first = claims(&client, "test-game");
    let mut session = ClientSession::connect(
        entry.local_addr(),
        &issuer.sign(&first).unwrap(),
        &client,
        exit.exit_public_key().unwrap(),
        TIMEOUT,
    )
    .await
    .unwrap();
    echo_round_trip(&mut session, echo, b"through both")
        .await
        .unwrap();
    let renewed = issuer
        .sign(&Claims {
            expires_at: first.expires_at + 900,
            issued_at: now_unix(),
            ..first
        })
        .unwrap();
    session.hello(&renewed, TIMEOUT).await.unwrap();
    echo_round_trip(&mut session, echo, b"after renewal")
        .await
        .unwrap();
    assert_eq!(exit.metrics().drops(DropReason::MalformedFrame), 0);
    assert_eq!(exit_sessions(&entry), 0, "the entry's own exit is not used");
}

#[tokio::test]
async fn either_of_the_last_two_handshakes_can_confirm_a_return_path() {
    // A failover sends successive initiations through different relays, and
    // the client accepts the answer to its current or its previous one, so
    // both can complete. The return path follows whichever is confirmed
    // last, and a straggler from the older one never moves it back.
    use pingpath_tunnel::core::{Event, TunnelCore};
    let s = setup(Limits::default()).await;
    let client = WireGuardKeys::generate().unwrap();
    let grant = claims(&client, "test-game");
    let token = s.issuer.sign(&grant).unwrap();
    let exit = s.exit.local_addr();
    let mut core = TunnelCore::new(
        grant.session_id,
        client.secret.clone(),
        s.exit.exit_public_key().unwrap(),
        std::net::Ipv4Addr::new(10, 0, 0, 2),
    )
    .unwrap();
    let a = UdpSocket::bind((LOCALHOST, 0)).await.unwrap();
    let b = UdpSocket::bind((LOCALHOST, 0)).await.unwrap();
    async fn next(socket: &UdpSocket) -> Vec<u8> {
        let mut buf = [0u8; 2048];
        let (n, _) = tokio::time::timeout(TIMEOUT, socket.recv_from(&mut buf))
            .await
            .expect("an answer")
            .unwrap();
        buf[..n].to_vec()
    }
    async fn echo_lands_on(
        core: &mut TunnelCore,
        send: &UdpSocket,
        expect: &UdpSocket,
        exit: SocketAddr,
    ) {
        let (seq, Some(frame)) = core.echo().unwrap() else {
            panic!("tunnel not up");
        };
        send.send_to(&frame, exit).await.unwrap();
        let reply = next(expect).await;
        let events = core.handle(&reply, Some(exit.ip())).events;
        assert!(events.contains(&Event::Echo(seq)), "{events:?}");
    }

    a.send_to(&core.hello(&token), exit).await.unwrap();
    let ack = next(&a).await;
    assert!(matches!(
        core.handle(&ack, Some(exit.ip())).events[..],
        [Event::Accepted { .. }]
    ));
    let first = core.handshake_initiation().unwrap();
    a.send_to(&first, exit).await.unwrap();
    let answer_a = next(&a).await;
    let second = core.handshake_initiation().unwrap();
    b.send_to(&second, exit).await.unwrap();
    let answer_b = next(&b).await;

    // The older answer completes first: confirming it moves the path to a.
    let handled = core.handle(&answer_a, Some(exit.ip()));
    assert!(handled.events.contains(&Event::HandshakeCompleted));
    for frame in &handled.send {
        a.send_to(frame, exit).await.unwrap();
    }
    echo_lands_on(&mut core, &a, &a, exit).await;

    // Then the newer one, through b: the path follows it.
    let handled = core.handle(&answer_b, Some(exit.ip()));
    assert!(handled.events.contains(&Event::HandshakeCompleted));
    for frame in &handled.send {
        b.send_to(frame, exit).await.unwrap();
    }
    echo_lands_on(&mut core, &b, &b, exit).await;

    // Data still arriving from a doesn't take the path back.
    echo_lands_on(&mut core, &a, &b, exit).await;
}
