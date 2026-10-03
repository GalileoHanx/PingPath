//! Series of latency probes to one target: ICMP echo, or TCP connection time
//! for targets that drop ICMP.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use crate::icmp::{Pinger, Reply};
use crate::stats::Stats;

#[derive(Debug, Clone, Copy)]
pub struct SeriesOptions {
    pub count: u32,
    pub interval: Duration,
    pub timeout: Duration,
}

/// Pings `dst` `count` times. Fails only if no probe could be sent at all.
pub fn icmp_series(dst: Ipv4Addr, options: &SeriesOptions) -> io::Result<Stats> {
    let mut pinger = Pinger::new()?;
    let mut last_error = None;
    let samples = paced(options, |seq| {
        match pinger.send(dst, 64, seq, options.timeout) {
            Ok(Reply::Echo { from, rtt }) if from == dst => Some(rtt),
            Ok(other) => {
                tracing::debug!(%dst, seq, reply = ?other, "no echo reply");
                None
            }
            Err(e) => {
                tracing::debug!(%dst, seq, error = %e, "probe failed");
                last_error = Some(e);
                None
            }
        }
    });
    finish(samples, last_error)
}

/// Times `count` TCP connections to `dst`. A refused connection still
/// measures a round trip (SYN out, RST back), so it counts as a reply.
pub fn tcp_series(dst: SocketAddr, options: &SeriesOptions) -> io::Result<Stats> {
    let mut last_error = None;
    let samples = paced(options, |seq| {
        let start = Instant::now();
        match TcpStream::connect_timeout(&dst, options.timeout) {
            Ok(_) => Some(start.elapsed()),
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => Some(start.elapsed()),
            Err(e) => {
                tracing::debug!(%dst, seq, error = %e, "connection failed");
                if e.kind() != io::ErrorKind::TimedOut {
                    last_error = Some(e);
                }
                None
            }
        }
    });
    finish(samples, last_error)
}

/// Runs `probe` `count` times, starting one every `interval`.
pub(crate) fn paced<T>(options: &SeriesOptions, mut probe: impl FnMut(u16) -> T) -> Vec<T> {
    let mut results = Vec::with_capacity(options.count as usize);
    for seq in 0..options.count {
        let start = Instant::now();
        results.push(probe(seq as u16));
        if seq + 1 < options.count {
            thread::sleep(options.interval.saturating_sub(start.elapsed()));
        }
    }
    results
}

/// Turns a series that never got through because of local errors (no route,
/// permission) into that error rather than "100% loss".
fn finish(samples: Vec<Option<Duration>>, last_error: Option<io::Error>) -> io::Result<Stats> {
    let stats = Stats::from_samples(&samples);
    match last_error {
        Some(e) if stats.received == 0 => Err(e),
        _ => Ok(stats),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn quick(count: u32) -> SeriesOptions {
        SeriesOptions {
            count,
            interval: Duration::from_millis(5),
            timeout: Duration::from_millis(500),
        }
    }

    #[test]
    fn tcp_series_measures_a_listening_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stats = tcp_series(addr, &quick(3)).unwrap();
        assert_eq!(stats.received, 3);
        assert!(stats.avg_ms.unwrap() < 100.0);
    }

    // Windows retries a refused connection for about two seconds before
    // reporting it, so there a closed port reads as a timeout instead.
    #[cfg(not(windows))]
    #[test]
    fn tcp_series_counts_a_refused_connection_as_a_reply() {
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        // The listener is dropped, so the port refuses connections.
        let stats = tcp_series(addr, &quick(2)).unwrap();
        assert_eq!(stats.received, 2);
    }
}
