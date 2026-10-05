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

use pingpath_games::{PingTarget, RoutingPolicy};
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
    /// What the control plane last said it ignored, to warn on changes only.
    ignored: String,
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

/// `sent` and `received` let the control plane pool a minute of rounds,
/// so one lost probe in a round of three doesn't read as 33% loss.
#[derive(Debug, Clone, Serialize)]
struct RegionSample {
    game: String,
    region: String,
    rtt_ms: Option<f64>,
    loss_pct: f64,
    sent: u32,
    received: u32,
}

#[derive(Debug, Serialize)]
struct Sample {
    peer_id: u32,
    rtt_ms: Option<f64>,
    loss_pct: f64,
    sent: u32,
    received: u32,
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
    // Region targets are measured with blocking pings on another thread. A
    // round of them can outlast a relay round (slow or silent targets), so
    // its results go with whichever report follows it, and the relay rows
    // never wait for them.
    let mut region_round: Option<tokio::task::JoinHandle<Vec<RegionSample>>> = None;
    loop {
        interval.tick().await;
        if region_round.is_none() && !regions.is_empty() {
            let regions = regions.clone();
            region_round = Some(tokio::task::spawn_blocking(move || {
                measure_regions(&regions)
            }));
        }
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
        let sent_count = config.probes_per_round.max(1);
        let sent = f64::from(sent_count);
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
                sent: sent_count,
                received: (rtts.len() as u32).min(sent_count),
            });
        }
        shared.metrics.set_peer_latency(latencies);
        let regions = match region_round.take_if(|task| task.is_finished()) {
            Some(task) => task.await.unwrap_or_default(),
            None => Vec::new(),
        };
        if !regions.is_empty() {
            shared.metrics.set_region_latency(
                regions
                    .iter()
                    .map(|r| RegionLatency {
                        game: r.game.clone(),
                        region: r.region.clone(),
                        rtt_ms: r.rtt_ms,
                        loss_pct: r.loss_pct,
                    })
                    .collect(),
            );
        }
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

/// A game region this exit serves, with that game's ping targets there.
#[derive(Debug, Clone)]
struct RegionTargets {
    game: String,
    region: String,
    targets: Vec<PingTarget>,
}

/// For an exit: each (game, region) it serves, for games that may be
/// routed. Region IDs are free-form per game, so each game's own targets
/// are measured; and servers of games that are never routed are never
/// probed (their operators could flag the exit's address).
fn region_targets(shared: &Shared) -> Vec<RegionTargets> {
    if !shared.config.has_role(Role::Exit) {
        return Vec::new();
    }
    let mut games: Vec<_> = shared
        .games
        .values()
        .filter(|g| g.status.routing_policy() != RoutingPolicy::Refuse)
        .collect();
    games.sort_by(|a, b| a.id.cmp(&b.id));
    let mut found = Vec::new();
    for game in games {
        for region in &shared.config.regions {
            let Some(r) = game.region(region) else {
                continue;
            };
            if r.ping_targets.is_empty() {
                tracing::warn!(game = %game.id, %region, "the game lists no ping targets for this region; clients can't estimate paths through this exit");
                continue;
            }
            found.push(RegionTargets {
                game: game.id.clone(),
                region: region.clone(),
                targets: r.ping_targets.clone(),
            });
        }
    }
    found
}

/// One round of region measurements: the best target's round trip per
/// game region, picked the same way the probe and the client pick it.
fn measure_regions(regions: &[RegionTargets]) -> Vec<RegionSample> {
    regions
        .iter()
        .map(|r| {
            let best = measure::best_of(&r.targets, &REGION_SERIES);
            let loss_pct = best.as_ref().map_or(100.0, |(_, stats)| stats.loss_pct);
            let sent = REGION_SERIES.count;
            RegionSample {
                game: r.game.clone(),
                region: r.region.clone(),
                rtt_ms: best.as_ref().and_then(|(_, stats)| stats.avg_ms),
                loss_pct,
                sent,
                received: (f64::from(sent) * (1.0 - loss_pct / 100.0)).round() as u32,
            }
        })
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
        let mut response = agent
            .post(&url)
            .header("Authorization", &format!("Bearer {key}"))
            .header("Content-Type", "application/json")
            .send(&body)?;
        // Samples the control plane didn't store (a game or region it
        // doesn't route, mid-deploy): the rest of the report counted.
        let answer: serde_json::Value = response
            .body_mut()
            .read_to_string()
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Ok::<_, ureq::Error>(match answer.get("ignored") {
            Some(ignored) if ignored.as_array().is_some_and(|a| !a.is_empty()) => {
                ignored.to_string()
            }
            _ => String::new(),
        })
    })
    .await;
    match result {
        Ok(Ok(ignored)) => {
            tracing::debug!("latency reported to the control plane");
            let mut probes = shared.probes.lock().expect("probe state lock");
            if probes.ignored != ignored {
                if ignored.is_empty() {
                    tracing::info!("the control plane accepts every latency sample again");
                } else {
                    tracing::warn!(%ignored, "the control plane ignored some latency samples: check this relay's games and regions against its");
                }
                probes.ignored = ignored;
            }
        }
        Ok(Err(e)) => tracing::warn!(error = %e, "cannot report latency to the control plane"),
        Err(e) => tracing::warn!(error = %e, "latency report task failed"),
    }
}
