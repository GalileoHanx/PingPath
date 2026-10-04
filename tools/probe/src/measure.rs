//! Series of latency probes to one target: ICMP echo, or TCP connection time
//! for targets that drop ICMP.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use pingpath_games::PingTarget;

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

/// Measures one of a game definition's ping targets: ICMP echo for an
/// IPv4 address, TCP connection time for `address:port`.
pub fn target_series(target: &PingTarget, options: &SeriesOptions) -> io::Result<Stats> {
    match *target {
        PingTarget::Icmp(IpAddr::V4(ip)) => icmp_series(ip, options),
        PingTarget::Icmp(IpAddr::V6(_)) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "IPv6 ICMP isn't supported yet; use a TCP target",
        )),
        PingTarget::Tcp(addr) => tcp_series(addr, options),
    }
}

/// Measures every target and returns the one with the lowest average round
/// trip: a region's figure, picked the same way `probe_region` picks it.
/// Targets that can't be measured or never answer are skipped; `None` if
/// none answered. Used by exits for their regions and by the client for
/// the direct route, so both sides compare like with like.
pub fn best_of(targets: &[PingTarget], options: &SeriesOptions) -> Option<(PingTarget, Stats)> {
    targets
        .iter()
        .filter_map(|target| match target_series(target, options) {
            Ok(stats) if stats.avg_ms.is_some() => Some((*target, stats)),
            Ok(_) => None,
            Err(e) => {
                tracing::debug!(%target, error = %e, "ping target could not be measured");
                None
            }
        })
        .min_by(|(_, a), (_, b)| {
            a.avg_ms
                .unwrap_or(f64::INFINITY)
                .total_cmp(&b.avg_ms.unwrap_or(f64::INFINITY))
        })
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
    fn best_of_picks_the_fastest_answering_target() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let open = PingTarget::Tcp(listener.local_addr().unwrap());
        // IPv6 ICMP can't be measured yet, so it is skipped.
        let silent = PingTarget::Icmp("2001:db8::1".parse().unwrap());
        let options = SeriesOptions {
            count: 2,
            interval: Duration::from_millis(5),
            timeout: Duration::from_millis(200),
        };
        let (best, stats) = best_of(&[silent, open], &options).unwrap();
        assert_eq!(best, open);
        assert_eq!(stats.received, 2);
        assert!(best_of(&[silent], &options).is_none());
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
