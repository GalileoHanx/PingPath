//! The player's own measurements (docs/decisions/009-path-selection-and-failover.md):
//! the direct route to the game region through its ping targets, and the
//! round trip to each relay with PROBE frames (ADR 004).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use pingpath_games::Region;
use pingpath_probe::measure::{self, SeriesOptions};
use pingpath_tunnel::frame::{self, Frame};
use tokio::net::UdpSocket;

use crate::select::Leg;

/// PROBEs are padded like the relays' own, to a typical game packet size.
const PROBE_PADDING: usize = 64;

/// The direct route to a game region. A trait so engine tests on loopback
/// can stand in a realistic figure.
pub trait DirectProbe: Send + Sync + 'static {
    /// Blocking: the player's round trip to the region, or `None` if no
    /// ping target answered.
    fn measure(&self, region: &Region) -> Option<Leg>;

    /// Blocking, and done within about `within`: whether any of the
    /// region's ping targets answers at all. A failing path asks this to
    /// tell a dead relay from a local outage before its deadline.
    fn reachable(&self, region: &Region, within: Duration) -> bool {
        let _ = within;
        self.measure(region).is_some()
    }
}

/// The region's ping targets, measured like `pingpath-probe` and the exits
/// measure them (the fastest answering target).
#[derive(Debug, Clone, Copy)]
pub struct PingTargets {
    pub series: SeriesOptions,
}

impl Default for PingTargets {
    fn default() -> Self {
        Self {
            series: SeriesOptions {
                count: 5,
                interval: Duration::from_millis(200),
                timeout: Duration::from_secs(1),
            },
        }
    }
}

impl DirectProbe for PingTargets {
    fn measure(&self, region: &Region) -> Option<Leg> {
        let (target, stats) = measure::best_of(&region.ping_targets, &self.series)?;
        tracing::info!(region = %region.id, %target, avg_ms = ?stats.avg_ms, loss_pct = stats.loss_pct, "direct route measured");
        Some(Leg {
            rtt_ms: stats.avg_ms?,
            loss_pct: stats.loss_pct,
            samples: self.series.count,
        })
    }

    /// One ping to every target at once, each waiting at most `within`.
    fn reachable(&self, region: &Region, within: Duration) -> bool {
        let once = SeriesOptions {
            count: 1,
            interval: Duration::ZERO,
            timeout: within,
        };
        std::thread::scope(|scope| {
            let pings: Vec<_> = region
                .ping_targets
                .iter()
                .map(|target| {
                    scope.spawn(move || {
                        measure::target_series(target, &once).is_ok_and(|s| s.avg_ms.is_some())
                    })
                })
                .collect();
            // The scope waits for the rest anyway, each at most `within`.
            pings.into_iter().any(|ping| ping.join().unwrap_or(false))
        })
    }
}

/// A fixed figure (tests, and the netns harness).
#[derive(Debug, Clone, Copy)]
pub struct Fixed(pub Option<Leg>);

impl DirectProbe for Fixed {
    fn measure(&self, _region: &Region) -> Option<Leg> {
        self.0
    }
}

/// How many PROBEs to send each relay and how long to wait.
#[derive(Debug, Clone, Copy)]
pub struct ProbePlan {
    pub probes: u32,
    pub spacing: Duration,
    pub timeout: Duration,
}

impl Default for ProbePlan {
    /// Ten, so a leg is judged lossy only when two are lost (select.rs).
    fn default() -> Self {
        Self {
            probes: 10,
            spacing: Duration::from_millis(100),
            timeout: Duration::from_secs(1),
        }
    }
}

/// The player's round trip and loss to each relay, by ID. Relays that never
/// answered are left out.
pub async fn probe_relays(relays: &[(u32, SocketAddr)], plan: ProbePlan) -> HashMap<u32, Leg> {
    let (v4, v6) = tokio::join!(UdpSocket::bind("0.0.0.0:0"), UdpSocket::bind("[::]:0"));
    let (v4, v6) = (v4.ok(), v6.ok());
    let mut nonce = u64::from_le_bytes(
        pingpath_tunnel::keys::random_32().unwrap_or([7; 32])[..8]
            .try_into()
            .expect("8 bytes"),
    );
    let mut pending: HashMap<u64, (u32, SocketAddr, Instant)> = HashMap::new();
    let mut replies: HashMap<u32, Vec<f64>> = HashMap::new();
    for round in 0..plan.probes {
        for &(id, addr) in relays {
            let socket = if addr.is_ipv4() {
                v4.as_ref()
            } else {
                v6.as_ref()
            };
            let Some(socket) = socket else { continue };
            nonce = nonce.wrapping_add(1);
            if socket
                .send_to(&frame::probe(nonce, PROBE_PADDING), addr)
                .await
                .is_ok()
            {
                pending.insert(nonce, (id, addr, Instant::now()));
            }
        }
        let until = Instant::now()
            + if round + 1 < plan.probes {
                plan.spacing
            } else {
                plan.timeout
            };
        collect(until, &mut pending, &mut replies, v4.as_ref(), v6.as_ref()).await;
    }

    let sent = f64::from(plan.probes.max(1));
    relays
        .iter()
        .filter_map(|&(id, _)| {
            let mut rtts = replies.remove(&id)?;
            rtts.sort_by(f64::total_cmp);
            let median = rtts[rtts.len() / 2];
            Some((
                id,
                Leg {
                    rtt_ms: (median * 100.0).round() / 100.0,
                    loss_pct: ((sent - rtts.len() as f64).max(0.0) / sent * 1000.0).round() / 10.0,
                    samples: plan.probes,
                },
            ))
        })
        .collect()
}

/// Receives PROBE_REPLYs until `until`, matching each to its probe by
/// nonce and sender.
async fn collect(
    until: Instant,
    pending: &mut HashMap<u64, (u32, SocketAddr, Instant)>,
    replies: &mut HashMap<u32, Vec<f64>>,
    v4: Option<&UdpSocket>,
    v6: Option<&UdpSocket>,
) {
    let (mut buf4, mut buf6) = ([0u8; 2048], [0u8; 2048]);
    loop {
        let remaining = until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        let received = tokio::time::timeout(remaining, async {
            tokio::select! {
                Some((n, from)) = recv(v4, &mut buf4) => Some((buf4[..n].to_vec(), from)),
                Some((n, from)) = recv(v6, &mut buf6) => Some((buf6[..n].to_vec(), from)),
                else => None,
            }
        })
        .await;
        let Ok(Some((datagram, from))) = received else {
            return;
        };
        let Ok(Frame::ProbeReply { body }) = Frame::parse(&datagram) else {
            continue;
        };
        let Some(nonce) = frame::probe_nonce(body) else {
            continue;
        };
        if let Some((id, addr, sent)) = pending.remove(&nonce) {
            if addr == from {
                replies
                    .entry(id)
                    .or_default()
                    .push(sent.elapsed().as_secs_f64() * 1000.0);
            }
        }
    }
}

/// On Windows, a PROBE to a relay whose port is closed (restarting, say)
/// makes the next receive fail with WSAECONNRESET, though the socket still
/// works: one dead relay must not end the round for the others. Linux
/// ignores such ICMP errors on unconnected sockets.
async fn recv(socket: Option<&UdpSocket>, buf: &mut [u8]) -> Option<(usize, SocketAddr)> {
    let Some(socket) = socket else {
        return std::future::pending().await;
    };
    for _ in 0..RECV_ERRORS {
        match socket.recv_from(buf).await {
            Ok(received) => return Some(received),
            Err(error) => tracing::debug!(%error, "PROBE receive failed"),
        }
    }
    std::future::pending().await
}

/// Errors in a row before a socket is given up for the round (each comes
/// from one ICMP message, so a few dead relays never reach it).
const RECV_ERRORS: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_dead_relay_does_not_spoil_the_others() {
        let live = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let live_addr = live.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                let Ok((n, from)) = live.recv_from(&mut buf).await else {
                    continue;
                };
                if let Ok(Frame::Probe { body }) = Frame::parse(&buf[..n]) {
                    let _ = live.send_to(&frame::probe_reply(body), from).await;
                }
            }
        });
        // A port nobody listens on: on Windows its ICMP errors reach the
        // probe socket before the live relay's replies in every round.
        let dead = std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let legs = probe_relays(
            &[(1, dead), (2, live_addr)],
            ProbePlan {
                probes: 5,
                spacing: Duration::from_millis(20),
                timeout: Duration::from_millis(300),
            },
        )
        .await;
        assert!(!legs.contains_key(&1));
        assert_eq!(legs[&2].loss_pct, 0.0, "{legs:?}");
    }
}
