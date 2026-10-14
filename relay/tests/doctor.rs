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

// Threads of its own for the relay: on one thread, writes that the
// kernel's buffers absorb never yield, and the relay never gets to read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
    // Far more than loopback's socket buffers hold.
    while sent < 64_000_000 {
        if timeout(TIMEOUT, up.write_all(&chunk))
            .await
            .unwrap()
            .is_err()
        {
            break;
        }
        sent += chunk.len();
    }
    assert!(sent < 64_000_000, "the relay kept reading past the budget");
    let received = metrics.doctor_bytes_received.load(Ordering::Relaxed);
    assert!(
        (1_000_000..1_000_000 + 2 * 64 * 1024).contains(&received),
        "{received}"
    );
    assert_eq!(metrics.doctor_tests.load(Ordering::Relaxed), 1);
}

/// The client measures upload by what arrived: the relay reports its
/// running count on the connection.
#[tokio::test]
async fn an_upload_connection_hears_how_much_arrived() {
    let issuer = Issuer::from_seed(&keys::random_32().unwrap()).unwrap();
    let relay = relay(&issuer).await;
    let addr = relay.doctor_addr().unwrap();
    let good = ticket(&issuer, RELAY_ID, 1_000_000, 60);
    let (answer, mut up) = open(addr, &good, UP).await;
    assert_eq!(answer, ACCEPTED);
    up.write_all(&[7u8; 100_000]).await.unwrap();
    let mut report = [0u8; 8];
    let mut last = 0;
    while last < 100_000 {
        timeout(TIMEOUT, up.read_exact(&mut report))
            .await
            .expect("a report within the timeout")
            .unwrap();
        let received = u64::from_be_bytes(report);
        assert!(
            (last..=100_000).contains(&received),
            "{received} after {last}"
        );
        last = received;
    }
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

#[tokio::test]
async fn a_client_that_stops_reading_is_cut_off_when_its_ticket_expires() {
    let issuer = Issuer::from_seed(&keys::random_32().unwrap()).unwrap();
    let relay = relay(&issuer).await;
    let addr = relay.doctor_addr().unwrap();
    let pinger = UdpSocket::bind((LOCALHOST, 0)).await.unwrap();
    // Valid for two more seconds, with a budget it can't use up.
    let short = ticket(&issuer, RELAY_ID, 1 << 40, 2);
    let (answer, _stalled) = open(addr, &short, DOWN).await;
    assert_eq!(answer, ACCEPTED);
    assert!(
        echoed(&pinger, addr).await,
        "the echo answers while the test runs"
    );
    // Never read: the relay's writes stall, and the ticket's end must
    // still close the connection (and with it the echo).
    let started = std::time::Instant::now();
    loop {
        if !echoed(&pinger, addr).await {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(12),
            "the stalled connection outlived its ticket"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn a_relay_runs_a_few_tests_at_once() {
    let issuer = Issuer::from_seed(&keys::random_32().unwrap()).unwrap();
    let relay = relay(&issuer).await;
    let addr = relay.doctor_addr().unwrap();
    let mut running = Vec::new();
    let mut tickets = Vec::new();
    for _ in 0..pingpath_relay::doctor::MAX_TESTS {
        let ticket = ticket(&issuer, RELAY_ID, 1_000_000, 60);
        let (answer, stream) = open(addr, &ticket, UP).await;
        assert_eq!(answer, ACCEPTED);
        running.push(stream);
        tickets.push(ticket);
    }
    let one_more = ticket(&issuer, RELAY_ID, 1_000_000, 60);
    assert_eq!(open(addr, &one_more, UP).await.0, REFUSED, "relay busy");
    // A test already running may still open its other connections.
    assert_eq!(open(addr, &tickets[0], UP).await.0, ACCEPTED);
    // When one ends, another may start.
    drop(running.remove(1));
    let mut admitted = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if open(addr, &one_more, UP).await.0 == ACCEPTED {
            admitted = true;
            break;
        }
    }
    assert!(admitted, "a finished test frees its place");
}

#[tokio::test]
async fn connections_without_a_ticket_are_limited_per_network() {
    let issuer = Issuer::from_seed(&keys::random_32().unwrap()).unwrap();
    let relay = relay(&issuer).await;
    let addr = relay.doctor_addr().unwrap();
    // Sixty connections at once from one network, none showing a ticket:
    // past the burst, the relay closes them without waiting for one.
    let mut streams = Vec::new();
    for _ in 0..60 {
        streams.push(TcpStream::connect(addr).await.unwrap());
    }
    // Well inside the 5 s the relay gives a held connection to show its
    // ticket, so only the refused ones are closed by now.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let closed = streams
        .iter()
        .filter(|stream| {
            let mut byte = [0u8; 1];
            match stream.try_read(&mut byte) {
                Ok(0) => true,
                Err(e) => e.kind() != std::io::ErrorKind::WouldBlock,
                Ok(_) => false,
            }
        })
        .count();
    assert!((10..=30).contains(&closed), "{closed} of 60 closed at once");
    assert!(relay.metrics().doctor_refused.load(Ordering::Relaxed) >= 10);
}
