//! PLAN.md rule 9: relays never log a client's address. This runs a
//! session through its whole life (address check, handshake, traffic, a
//! refused destination, a new address, renewal, refusals) with every log
//! level switched on, and checks no line holds the client's IP and port.

mod common;

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use pingpath_relay::config::Limits;
use pingpath_tunnel::client::{ClientError, ClientSession};
use pingpath_tunnel::frame::session_hex;
use pingpath_tunnel::keys::WireGuardKeys;
use pingpath_tunnel::token::now_unix;
use tracing::Level;

/// Collects everything the relays log.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn relays_never_log_client_addresses() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let s = setup(Limits::default()).await;
    let exit_key = s.exit.exit_public_key().unwrap();
    let mut client_ports = Vec::new();

    // Through the entry: traffic, the in-tunnel echo, a refused destination.
    let client = WireGuardKeys::generate().unwrap();
    let mut grant = claims(&client, "test-game");
    let token = s.issuer.sign(&grant).unwrap();
    let mut session =
        ClientSession::connect(s.entry.local_addr(), &token, &client, exit_key, TIMEOUT)
            .await
            .unwrap();
    client_ports.push(session.local_addr().unwrap().port());
    echo_round_trip(&mut session, s.echo, b"through the entry")
        .await
        .unwrap();
    session.echo(TIMEOUT).await.unwrap();
    session
        .send_udp(40_001, s.forbidden, b"not allowed")
        .await
        .unwrap();

    // A new address: dropped until it says HELLO again, then renewed.
    session.rebind().await.unwrap();
    client_ports.push(session.local_addr().unwrap().port());
    session.send_udp(40_000, s.echo, b"dropped").await.unwrap();
    session.hello(&token, TIMEOUT).await.unwrap();
    grant.expires_at = now_unix() + 1800;
    session
        .hello(&s.issuer.sign(&grant).unwrap(), TIMEOUT)
        .await
        .unwrap();
    echo_round_trip(&mut session, s.echo, b"after moving")
        .await
        .unwrap();

    // Straight to the exit, moving address with a new handshake.
    let (mut direct, _) = s
        .connect_via(s.exit.local_addr(), "test-game")
        .await
        .unwrap();
    client_ports.push(direct.local_addr().unwrap().port());
    echo_round_trip(&mut direct, s.echo, b"direct")
        .await
        .unwrap();
    direct.rebind().await.unwrap();
    client_ports.push(direct.local_addr().unwrap().port());
    direct.handshake(TIMEOUT).await.unwrap();
    echo_round_trip(&mut direct, s.echo, b"direct, moved")
        .await
        .unwrap();

    // Refusals.
    let refused = s.connect_via(s.entry.local_addr(), "untested-game").await;
    assert!(matches!(refused, Err(ClientError::Refused(_))));
    let mut old = claims(&client, "test-game");
    old.expires_at = now_unix() - 600;
    let expired = s.issuer.sign(&old).unwrap();
    let refused =
        ClientSession::connect(s.exit.local_addr(), &expired, &client, exit_key, TIMEOUT).await;
    assert!(matches!(refused, Err(ClientError::Refused(_))));

    tokio::time::sleep(Duration::from_millis(200)).await;
    let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains(&session_hex(&session.session_id())),
        "the relays' logs were not captured"
    );
    for port in client_ports {
        for needle in [
            format!("127.0.0.1:{port}"),
            format!(":{port},"),
            format!(":{port}}}"),
            format!("\"{port}\""),
        ] {
            let leak = logs.lines().find(|line| line.contains(&needle));
            assert!(
                leak.is_none(),
                "a client address reached the log: {}",
                leak.unwrap()
            );
        }
    }
}
