//! The doctor listener (docs/decisions/011-home-network-doctor.md): load
//! for bufferbloat tests only with a valid ticket for this relay, within
//! its byte budget and connection limit, and the UDP echo only to an
//! address with a load connection open.

mod common;

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use pingpath_relay::config::{DoctorListener, Limits, Role};
use pingpath_relay::{Relay, RelayOptions};
use pingpath_tunnel::keys;
use pingpath_tunnel::ticket::{self, ACCEPTED, DOWN, REFUSED, Ticket, UP};
use pingpath_tunnel::token::{Issuer, now_unix};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::timeout;

const RELAY_ID: u32 = 7;

async fn relay(issuer: &Issuer) -> Relay {
    let mut config = config(
        RELAY_ID,
        vec![Role::Entry],
        issuer,
        Vec::new(),
        Limits::default(),
    );
    config.doctor = Some(DoctorListener {
        listen: SocketAddr::from((LOCALHOST, 0)),
    });
    Relay::start(RelayOptions {
        config,
        games: Vec::new(),
        exit_secret: None,
        control_api_key: None,
    })
    .await
    .unwrap()
}

fn ticket(issuer: &Issuer, relay_id: u32, max_bytes: u64, expires_in: i64) -> Vec<u8> {
    let now = now_unix();
    issuer.sign_ticket(&Ticket {
        id: keys::random_32().unwrap()[..ticket::ID_LEN]
            .try_into()
            .unwrap(),
        relay_id,
        issued_at: now.saturating_sub(120),
        expires_at: now.saturating_add_signed(expires_in),
        max_bytes,
    })
}

/// Opens a load connection; the relay's answer byte, and the stream.
async fn open(addr: SocketAddr, ticket: &[u8], direction: u8) -> (u8, TcpStream) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut request = (ticket.len() as u16).to_be_bytes().to_vec();
    request.extend_from_slice(ticket);
    request.push(direction);
    stream.write_all(&request).await.unwrap();
    let answer = timeout(TIMEOUT, stream.read_u8()).await.unwrap().unwrap();
    (answer, stream)
}

async fn read_to_end(stream: &mut TcpStream) -> usize {
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0;
    loop {
        match timeout(TIMEOUT, stream.read(&mut buffer)).await.unwrap() {
            Ok(0) | Err(_) => return total,
            Ok(n) => total += n,
        }
    }
}

async fn echoed(socket: &UdpSocket, to: SocketAddr) -> bool {
    socket.send_to(b"ping 1", to).await.unwrap();
    let mut buffer = [0u8; 128];
    match timeout(Duration::from_millis(300), socket.recv_from(&mut buffer)).await {
        Ok(Ok((n, _))) => &buffer[..n] == b"ping 1",
        _ => false,
    }
}

#[tokio::test]
async fn load_needs_a_ticket_for_this_relay_and_stops_at_its_budget() {
    let issuer = Issuer::from_seed(&keys::random_32().unwrap()).unwrap();
    let relay = relay(&issuer).await;
    let addr = relay.doctor_addr().unwrap();
    let metrics = relay.metrics();

    // Another relay's ticket, an expired one, one from another issuer.
    let stranger = Issuer::from_seed(&keys::random_32().unwrap()).unwrap();
    for bad in [
        ticket(&issuer, RELAY_ID + 1, 1_000_000, 60),
        ticket(&issuer, RELAY_ID, 1_000_000, -120),
        ticket(&stranger, RELAY_ID, 1_000_000, 60),
    ] {
        assert_eq!(open(addr, &bad, DOWN).await.0, REFUSED);
    }
    // Not a ticket at all: the connection just closes.
    let mut junk = TcpStream::connect(addr).await.unwrap();
    junk.write_all(&[0, 3, 1, 2, 3, DOWN]).await.unwrap();
    assert_eq!(read_to_end(&mut junk).await, 0);
    assert_eq!(metrics.doctor_refused.load(Ordering::Relaxed), 4);

    // A megabyte down, shared by every connection on the ticket.
    let good = ticket(&issuer, RELAY_ID, 1_000_000, 60);
    let (answer, mut first) = open(addr, &good, DOWN).await;
    assert_eq!(answer, ACCEPTED);
    assert_eq!(read_to_end(&mut first).await, 1_000_000);
    let (answer, mut second) = open(addr, &good, DOWN).await;
    assert_eq!(answer, ACCEPTED);
    assert_eq!(read_to_end(&mut second).await, 0);
    assert_eq!(metrics.doctor_bytes_sent.load(Ordering::Relaxed), 1_000_000);

    // Up: the relay takes the budget and then hangs up.
    let (answer, mut up) = open(addr, &good, UP).await;
    assert_eq!(answer, ACCEPTED);
    let chunk = vec![7u8; 64 * 1024];
    let mut sent = 0usize;
    while sent < 4_000_000 {
        if timeout(TIMEOUT, up.write_all(&chunk))
            .await
            .unwrap()
            .is_err()
        {
            break;
        }
        sent += chunk.len();
    }
    assert!(sent < 4_000_000, "the relay kept reading past the budget");
    let received = metrics.doctor_bytes_received.load(Ordering::Relaxed);
    assert!(
        (1_000_000..1_000_000 + 2 * 64 * 1024).contains(&received),
        "{received}"
    );
    assert_eq!(metrics.doctor_tests.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn the_echo_answers_only_while_a_load_connection_is_open() {
    let issuer = Issuer::from_seed(&keys::random_32().unwrap()).unwrap();
    let relay = relay(&issuer).await;
    let addr = relay.doctor_addr().unwrap();
    let pinger = UdpSocket::bind((LOCALHOST, 0)).await.unwrap();
    assert!(!echoed(&pinger, addr).await);

    let good = ticket(&issuer, RELAY_ID, 1_000_000, 60);
    let (answer, up) = open(addr, &good, UP).await;
    assert_eq!(answer, ACCEPTED);
    assert!(echoed(&pinger, addr).await);
    // Too big to echo.
    pinger.send_to(&[1u8; 200], addr).await.unwrap();
    let mut buffer = [0u8; 256];
    assert!(
        timeout(Duration::from_millis(300), pinger.recv_from(&mut buffer))
            .await
            .is_err()
    );

    drop(up);
    let mut stopped = false;
    for _ in 0..20 {
        if !echoed(&pinger, addr).await {
            stopped = true;
            break;
        }
    }
    assert!(
        stopped,
        "the echo kept answering after the connection closed"
    );
}

#[tokio::test]
async fn one_ticket_holds_at_most_sixteen_connections() {
    let issuer = Issuer::from_seed(&keys::random_32().unwrap()).unwrap();
    let relay = relay(&issuer).await;
    let addr = relay.doctor_addr().unwrap();
    let good = ticket(&issuer, RELAY_ID, 1_000_000, 60);
    let mut open_ones = Vec::new();
    for _ in 0..16 {
        let (answer, stream) = open(addr, &good, UP).await;
        assert_eq!(answer, ACCEPTED);
        open_ones.push(stream);
    }
    assert_eq!(open(addr, &good, UP).await.0, REFUSED);
    // Another ticket is counted on its own.
    let other = ticket(&issuer, RELAY_ID, 1_000_000, 60);
    assert_eq!(open(addr, &other, UP).await.0, ACCEPTED);
}
