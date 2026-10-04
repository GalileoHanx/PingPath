//! Relay-to-relay probing: each relay measures the round trip and loss to
//! every other relay it knows, exposes the results as metrics and reports
//! them to the control plane, which uses them to recommend paths. Exits also
//! measure the game regions they serve, through the definitions' ping
//! targets, so clients can estimate a path's full round trip
//! (docs/decisions/009-path-selection-and-failover.md).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pingpath_games::PingTarget;
use pingpath_probe::measure::{self, SeriesOptions};
use pingpath_tunnel::frame;
use serde::Serialize;

use crate::config::Role;
use crate::metrics::{PeerLatency, RegionLatency};
use crate::relay::Shared;

const PROBE_SPACING: Duration = Duration::from_millis(200);
const LATE_REPLY_GRACE: Duration = Duration::from_secs(1);
/// PROBEs are padded to a typical game packet size.
const PROBE_PADDING: usize = 64;

#[derive(Debug, Default)]
pub struct ProbeState {
    next_nonce: u64,
    /// nonce -> (peer id, when it was sent)
    pending: HashMap<u64, (u32, Instant)>,
    /// peer id -> round trips this round
    replies: HashMap<u32, Vec<Duration>>,
}

/// Each region target is probed this way once a round.
const REGION_SERIES: SeriesOptions = SeriesOptions {
    count: 3,
    interval: Duration::from_millis(200),
    timeout: Duration::from_secs(1),
};

#[derive(Debug, Serialize)]
struct LatencyReport {
    relay_id: u32,
    samples: Vec<Sample>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    regions: Vec<RegionSample>,
}

#[derive(Debug, Serialize)]
struct RegionSample {
    region: String,
    rtt_ms: Option<f64>,
    loss_pct: f64,
}

#[derive(Debug, Serialize)]
struct Sample {
    peer_id: u32,
    rtt_ms: Option<f64>,
    loss_pct: f64,
}

impl Shared {
    /// A PROBE_REPLY: counts only if it answers a probe we sent to that peer.
    pub(crate) fn probe_reply(&self, body: &[u8], from: SocketAddr) {
        let Some(nonce) = frame::probe_nonce(body) else {
            return;
        };
        let Some(&peer) = self.peers_by_address.get(&from) else {
            return;
        };
        let mut state = self.probes.lock().expect("probe lock");
        if let Some((expected, sent)) = state.pending.remove(&nonce) {
            if expected == peer {
                state.replies.entry(peer).or_default().push(sent.elapsed());
            }
        }
    }
}

pub(crate) async fn probe_loop(shared: Arc<Shared>) {
    let config = shared.config.probe.clone();
    let peers = shared.config.relays.clone();
    let mut unreachable: HashMap<u32, bool> = HashMap::new();
    {
        let mut state = shared.probes.lock().expect("probe lock");
        state.next_nonce = u64::from_le_bytes(
            pingpath_tunnel::keys::random_32().unwrap_or([0; 32])[..8]
                .try_into()
                .expect("8 bytes"),
        );
    }
    let regions = region_targets(&shared);
    let mut interval = tokio::time::interval(Duration::from_secs(config.interval_secs));
    loop {
        interval.tick().await;
        // Region targets are measured with blocking probes on another thread
        // while the relay probes run here.
        let region_round = (!regions.is_empty()).then(|| {
            let regions = regions.clone();
            tokio::task::spawn_blocking(move || measure_regions(&regions))
        });
        for _ in 0..config.probes_per_round {
            for peer in &peers {
                let nonce = {
                    let mut state = shared.probes.lock().expect("probe lock");
                    state.next_nonce = state.next_nonce.wrapping_add(1);
                    let nonce = state.next_nonce;
                    state.pending.insert(nonce, (peer.id, Instant::now()));
                    nonce
                };
                shared
                    .send(&frame::probe(nonce, PROBE_PADDING), peer.address)
                    .await;
            }
            tokio::time::sleep(PROBE_SPACING).await;
        }
        tokio::time::sleep(LATE_REPLY_GRACE).await;

        let replies = {
            let mut state = shared.probes.lock().expect("probe lock");
            state.pending.clear();
            std::mem::take(&mut state.replies)
        };
        let sent = f64::from(config.probes_per_round.max(1));
        let mut latencies = Vec::new();
        let mut samples = Vec::new();
        for peer in &peers {
            let rtts = replies.get(&peer.id).map(Vec::as_slice).unwrap_or_default();
            let rtt_ms = (!rtts.is_empty())
                .then(|| {
                    rtts.iter().map(|d| d.as_secs_f64() * 1000.0).sum::<f64>() / rtts.len() as f64
                })
                .map(|ms| (ms * 100.0).round() / 100.0);
            let loss_pct = ((sent - rtts.len() as f64).max(0.0) / sent * 1000.0).round() / 10.0;

            // Log changes in reachability, not every round.
            let down = rtt_ms.is_none();
            if unreachable.insert(peer.id, down) != Some(down) {
                if down {
                    tracing::warn!(peer = %peer.name, "relay unreachable");
                } else {
                    tracing::info!(peer = %peer.name, rtt_ms = ?rtt_ms, loss_pct, "relay reachable");
                }
            }
            latencies.push(PeerLatency {
                peer: peer.name.clone(),
                rtt_ms,
                loss_pct,
            });
            samples.push(Sample {
                peer_id: peer.id,
                rtt_ms,
                loss_pct,
            });
        }
        shared.metrics.set_peer_latency(latencies);
        let regions = match region_round {
            Some(task) => task.await.unwrap_or_default(),
            None => Vec::new(),
        };
        shared.metrics.set_region_latency(
            regions
                .iter()
                .map(|r| RegionLatency {
                    region: r.region.clone(),
                    rtt_ms: r.rtt_ms,
                    loss_pct: r.loss_pct,
                })
                .collect(),
        );
        report(
            &shared,
            LatencyReport {
                relay_id: shared.config.id,
                samples,
                regions,
            },
        )
        .await;
    }
}

/// For an exit: each region it serves, with the ping targets every game
/// definition lists for that region.
fn region_targets(shared: &Shared) -> Vec<(String, Vec<PingTarget>)> {
    if !shared.config.has_role(Role::Exit) {
        return Vec::new();
    }
    let mut games: Vec<_> = shared.games.values().collect();
    games.sort_by(|a, b| a.id.cmp(&b.id));
    shared
        .config
        .regions
        .iter()
        .filter_map(|region| {
            let mut targets: Vec<PingTarget> = Vec::new();
            for target in games
                .iter()
                .filter_map(|g| g.region(region))
                .flat_map(|r| r.ping_targets.iter())
            {
                if !targets.contains(target) {
                    targets.push(*target);
                }
            }
            if targets.is_empty() {
                tracing::warn!(%region, "no game lists ping targets for this region; clients can't estimate paths through this exit");
                return None;
            }
            Some((region.clone(), targets))
        })
        .collect()
}

/// One round of region measurements: the best target's round trip per
/// region, picked the same way the probe and the client pick it.
fn measure_regions(regions: &[(String, Vec<PingTarget>)]) -> Vec<RegionSample> {
    regions
        .iter()
        .map(
            |(region, targets)| match measure::best_of(targets, &REGION_SERIES) {
                Some((_, stats)) => RegionSample {
                    region: region.clone(),
                    rtt_ms: stats.avg_ms,
                    loss_pct: stats.loss_pct,
                },
                None => RegionSample {
                    region: region.clone(),
                    rtt_ms: None,
                    loss_pct: 100.0,
                },
            },
        )
        .collect()
}

/// Sends the round's results to the control plane, if one is configured.
async fn report(shared: &Arc<Shared>, report: LatencyReport) {
    let (Some(control), Some(key)) = (
        shared.config.control.clone(),
        shared.control_api_key.clone(),
    ) else {
        return;
    };
    let url = format!("{}/v1/relay-latency", control.url.trim_end_matches('/'));
    let body = match serde_json::to_string(&report) {
        Ok(body) => body,
        Err(e) => return tracing::warn!(error = %e, "cannot encode the latency report"),
    };
    let result = tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into();
        agent
            .post(&url)
            .header("Authorization", &format!("Bearer {key}"))
            .header("Content-Type", "application/json")
            .send(&body)
            .map(|_| ())
    })
    .await;
    match result {
        Ok(Ok(())) => tracing::debug!("latency reported to the control plane"),
        Ok(Err(e)) => tracing::warn!(error = %e, "cannot report latency to the control plane"),
        Err(e) => tracing::warn!(error = %e, "latency report task failed"),
    }
}
