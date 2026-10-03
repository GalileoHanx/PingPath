//! UDP checks against a PingPath echo server: UDP latency, the address the
//! server sees us from, and whether WireGuard-shaped packets fare worse than
//! ordinary ones (a sign the ISP throttles or blocks VPN traffic).
//!
//! The server answers each datagram with the same bytes (never more, so it
//! can't amplify an attack) and rate-limits every source address.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use pingpath_diagnostics::Redacted;
use serde::Serialize;

use crate::measure::{SeriesOptions, paced};
use crate::stats::Stats;

const WHOAMI_REQUEST: &[u8] = b"PPW?";
const WHOAMI_REPLY: &[u8] = b"PPW=";
const PLAIN_MARKER: [u8; 4] = *b"PPE1";
/// First four bytes of a WireGuard handshake initiation: type 1, three zeros.
const WIREGUARD_MARKER: [u8; 4] = [1, 0, 0, 0];
/// A WireGuard handshake initiation is exactly this long.
const PACKET_LEN: usize = 148;
const MAX_PER_SOURCE_PER_SECOND: u32 = 100;
const MAX_DATAGRAM: usize = 1472;

#[derive(Debug, Clone, Serialize)]
pub struct UdpTest {
    /// The echo server, as given (redacted when it is a public address).
    pub endpoint: String,
    /// Our address as the echo server saw it, redacted.
    pub public_address: Option<String>,
    pub plain: Stats,
    pub wireguard_like: Stats,
    pub verdict: UdpVerdict,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UdpVerdict {
    Ok,
    Throttled,
    Blocked,
    Unreachable,
}

/// Runs the echo server until the process is stopped.
pub fn serve(bind: SocketAddr) -> io::Result<()> {
    let socket = UdpSocket::bind(bind)?;
    tracing::info!(address = %socket.local_addr()?, "UDP echo server listening");
    let mut buf = [0u8; 2048];
    let mut windows: HashMap<IpAddr, (Instant, u32)> = HashMap::new();
    let mut last_summary = Instant::now();
    let (mut answered, mut limited) = (0u64, 0u64);

    loop {
        let (n, from) = socket.recv_from(&mut buf)?;
        let now = Instant::now();
        let window = windows.entry(from.ip()).or_insert((now, 0));
        if now.duration_since(window.0) >= Duration::from_secs(1) {
            *window = (now, 0);
        }
        window.1 += 1;
        if window.1 > MAX_PER_SOURCE_PER_SECOND || n > MAX_DATAGRAM {
            limited += 1;
        } else {
            let reply = if &buf[..n] == WHOAMI_REQUEST {
                [WHOAMI_REPLY, from.to_string().as_bytes()].concat()
            } else {
                buf[..n].to_vec()
            };
            if let Err(e) = socket.send_to(&reply, from) {
                tracing::debug!(peer = %Redacted(from), error = %e, "cannot reply");
            }
            answered += 1;
        }
        if last_summary.elapsed() >= Duration::from_secs(60) {
            tracing::info!(
                answered,
                limited,
                sources = windows.len(),
                "echo server activity"
            );
            windows.retain(|_, (start, _)| start.elapsed() < Duration::from_secs(60));
            last_summary = Instant::now();
        }
    }
}

/// Measures plain and WireGuard-shaped UDP round trips to `endpoint`.
pub fn test(endpoint: SocketAddr, options: &SeriesOptions) -> io::Result<UdpTest> {
    let socket = UdpSocket::bind(if endpoint.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })?;
    socket.connect(endpoint)?;

    let public_address = whoami(&socket, options.timeout).map(|a| Redacted(a).to_string());
    let plain = series(&socket, PLAIN_MARKER, options)?;
    let wireguard_like = series(&socket, WIREGUARD_MARKER, options)?;
    let (verdict, detail) = judge(&plain, &wireguard_like);
    tracing::info!(?verdict, %detail, "UDP test finished");

    Ok(UdpTest {
        endpoint: Redacted(endpoint).to_string(),
        public_address,
        plain,
        wireguard_like,
        verdict,
        detail,
    })
}

fn whoami(socket: &UdpSocket, timeout: Duration) -> Option<SocketAddr> {
    socket.set_read_timeout(Some(timeout)).ok()?;
    let mut buf = [0u8; 128];
    for _ in 0..3 {
        socket.send(WHOAMI_REQUEST).ok()?;
        if let Ok(n) = socket.recv(&mut buf) {
            if let Some(addr) = buf[..n].strip_prefix(WHOAMI_REPLY) {
                return std::str::from_utf8(addr).ok()?.parse().ok();
            }
        }
    }
    None
}

fn series(socket: &UdpSocket, marker: [u8; 4], options: &SeriesOptions) -> io::Result<Stats> {
    let mut filler = RandomState::new().build_hasher().finish() | 1;
    let mut buf = [0u8; 2048];
    let samples = paced(options, |seq| {
        let mut packet = vec![0u8; PACKET_LEN];
        packet[..4].copy_from_slice(&marker);
        packet[4..8].copy_from_slice(&u32::from(seq).to_be_bytes());
        for byte in &mut packet[8..] {
            // xorshift: random-looking bytes, like encrypted traffic
            filler ^= filler << 13;
            filler ^= filler >> 7;
            filler ^= filler << 17;
            *byte = filler as u8;
        }
        let start = Instant::now();
        if socket.send(&packet).is_err() {
            return None;
        }
        loop {
            let remaining = options.timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() || socket.set_read_timeout(Some(remaining)).is_err() {
                return None;
            }
            match socket.recv(&mut buf) {
                Ok(n) if buf[..n] == packet[..] => return Some(start.elapsed()),
                Ok(_) => continue, // a late echo of an earlier packet
                Err(_) => return None,
            }
        }
    });
    Ok(Stats::from_samples(&samples))
}

fn judge(plain: &Stats, wireguard_like: &Stats) -> (UdpVerdict, String) {
    if plain.received == 0 && wireguard_like.received == 0 {
        return (
            UdpVerdict::Unreachable,
            "no echoes at all; is the echo server running and reachable?".into(),
        );
    }
    let extra_loss = wireguard_like.loss_pct - plain.loss_pct;
    if wireguard_like.received == 0 || (wireguard_like.loss_pct >= 90.0 && plain.loss_pct < 50.0) {
        return (
            UdpVerdict::Blocked,
            format!(
                "WireGuard-shaped packets lost {}% vs {}% for plain packets",
                wireguard_like.loss_pct, plain.loss_pct
            ),
        );
    }
    let slower = match (wireguard_like.avg_ms, plain.avg_ms) {
        (Some(wg), Some(p)) => wg > p * 1.5 + 20.0,
        _ => false,
    };
    if extra_loss >= 10.0 || slower {
        return (
            UdpVerdict::Throttled,
            format!(
                "WireGuard-shaped packets: {}% loss, {:?} ms average; plain: {}% loss, {:?} ms",
                wireguard_like.loss_pct, wireguard_like.avg_ms, plain.loss_pct, plain.avg_ms
            ),
        );
    }
    (
        UdpVerdict::Ok,
        "WireGuard-shaped packets fare the same as plain ones".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn echo_server_round_trip_and_whoami() {
        let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        thread::spawn(move || serve(bind));
        thread::sleep(Duration::from_millis(100));

        let options = SeriesOptions {
            count: 5,
            interval: Duration::from_millis(5),
            timeout: Duration::from_millis(500),
        };
        let result = test(bind, &options).unwrap();
        assert_eq!(result.plain.received, 5);
        assert_eq!(result.wireguard_like.received, 5);
        assert_eq!(result.verdict, UdpVerdict::Ok);
        // Loopback addresses are not redacted.
        assert!(result.public_address.unwrap().starts_with("127.0.0.1:"));
    }

    #[test]
    fn judges_blocking_and_throttling() {
        let stats = |loss: f64, avg: Option<f64>| Stats {
            sent: 10,
            received: (10.0 - loss / 10.0) as u32,
            loss_pct: loss,
            min_ms: avg,
            avg_ms: avg,
            p50_ms: avg,
            p95_ms: avg,
            max_ms: avg,
            jitter_ms: Some(1.0),
        };
        assert_eq!(
            judge(&stats(0.0, Some(80.0)), &stats(100.0, None)).0,
            UdpVerdict::Blocked
        );
        assert_eq!(
            judge(&stats(0.0, Some(80.0)), &stats(20.0, Some(82.0))).0,
            UdpVerdict::Throttled
        );
        assert_eq!(
            judge(&stats(0.0, Some(80.0)), &stats(0.0, Some(150.0))).0,
            UdpVerdict::Throttled
        );
        assert_eq!(
            judge(&stats(0.0, Some(80.0)), &stats(0.0, Some(81.0))).0,
            UdpVerdict::Ok
        );
    }
}
