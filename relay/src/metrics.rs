//! Counters and gauges, exposed in the Prometheus text format on the
//! relay's HTTP port (`/metrics`).

use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

/// Why a frame or packet was not forwarded. Every drop is counted under one
/// of these, so an operator (and a diagnostic report) can see what happened
/// without the relay logging client addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// Not a PingPath frame, or a truncated one.
    MalformedFrame,
    UnknownFrameType,
    /// DATA for a session this relay doesn't have (or no longer has).
    UnknownSession,
    /// DATA for a known session from an address that isn't the session's.
    WrongSource,
    RateLimited,
    BadToken,
    ExpiredToken,
    WrongExit,
    UnknownExit,
    GameNotRoutable,
    RelayFull,
    HelloFlood,
    ProbeFlood,
    /// Decrypted, but the destination isn't one of the game's servers.
    NotAllowlisted,
    NotUdp,
    NotIpv4,
    Fragment,
    MalformedPacket,
    FlowLimit,
    /// A WireGuard counter seen before: the second copy of a packet sent
    /// over two paths (Stability mode), or a replayed or forged copy. It is
    /// checked before decryption, so it proves nothing about the sender.
    Duplicate,
    /// Failed WireGuard checks: bad MAC or tag, wrong key, wrong index.
    TunnelRejected,
    SendFailed,
    /// A frame larger than the tunnel MTU allows.
    Oversize,
    /// A game server's reply too large to fit the tunnel.
    ReplyTooBig,
    /// The exit doesn't serve the token's region.
    WrongRegion,
    /// A destination that is one of our own relays.
    DeniedInfrastructure,
    /// Too much traffic to one game server across all sessions.
    DestinationLimited,
    /// A session opening flows faster than allowed.
    FlowRate,
    /// The return path has been silent too long to keep sending to it.
    StalePath,
}

impl DropReason {
    pub const ALL: [DropReason; 29] = [
        DropReason::MalformedFrame,
        DropReason::UnknownFrameType,
        DropReason::UnknownSession,
        DropReason::WrongSource,
        DropReason::RateLimited,
        DropReason::BadToken,
        DropReason::ExpiredToken,
        DropReason::WrongExit,
        DropReason::UnknownExit,
        DropReason::GameNotRoutable,
        DropReason::RelayFull,
        DropReason::HelloFlood,
        DropReason::ProbeFlood,
        DropReason::NotAllowlisted,
        DropReason::NotUdp,
        DropReason::NotIpv4,
        DropReason::Fragment,
        DropReason::MalformedPacket,
        DropReason::FlowLimit,
        DropReason::Duplicate,
        DropReason::TunnelRejected,
        DropReason::SendFailed,
        DropReason::Oversize,
        DropReason::ReplyTooBig,
        DropReason::WrongRegion,
        DropReason::DeniedInfrastructure,
        DropReason::DestinationLimited,
        DropReason::FlowRate,
        DropReason::StalePath,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DropReason::MalformedFrame => "malformed_frame",
            DropReason::UnknownFrameType => "unknown_frame_type",
            DropReason::UnknownSession => "unknown_session",
            DropReason::WrongSource => "wrong_source",
            DropReason::RateLimited => "rate_limited",
            DropReason::BadToken => "bad_token",
            DropReason::ExpiredToken => "expired_token",
            DropReason::WrongExit => "wrong_exit",
            DropReason::UnknownExit => "unknown_exit",
            DropReason::GameNotRoutable => "game_not_routable",
            DropReason::RelayFull => "relay_full",
            DropReason::HelloFlood => "hello_flood",
            DropReason::ProbeFlood => "probe_flood",
            DropReason::NotAllowlisted => "not_allowlisted",
            DropReason::NotUdp => "not_udp",
            DropReason::NotIpv4 => "not_ipv4",
            DropReason::Fragment => "fragment",
            DropReason::MalformedPacket => "malformed_packet",
            DropReason::FlowLimit => "flow_limit",
            DropReason::Duplicate => "duplicate",
            DropReason::TunnelRejected => "tunnel_rejected",
            DropReason::SendFailed => "send_failed",
            DropReason::Oversize => "oversize",
            DropReason::ReplyTooBig => "reply_too_big",
            DropReason::WrongRegion => "wrong_region",
            DropReason::DeniedInfrastructure => "denied_infrastructure",
            DropReason::DestinationLimited => "destination_limited",
            DropReason::FlowRate => "flow_rate",
            DropReason::StalePath => "stale_path",
        }
    }
}

/// Upper bounds of the processing-time histogram, in seconds.
const BUCKETS: [f64; 8] = [
    0.000_01, 0.000_05, 0.000_1, 0.000_25, 0.000_5, 0.001, 0.005, 0.025,
];

#[derive(Debug, Default)]
pub struct Metrics {
    drops: [AtomicU64; DropReason::ALL.len()],
    pub frames_received: AtomicU64,
    pub frames_forwarded: AtomicU64,
    /// Second copies an exit sent while a client duplicates (ADR 010).
    pub duplicated_frames: AtomicU64,
    pub duplicated_bytes: AtomicU64,
    pub bytes_received: AtomicU64,
    pub bytes_forwarded: AtomicU64,
    pub sessions_started: AtomicU64,
    pub sessions_ended: AtomicU64,
    pub entry_sessions: AtomicI64,
    pub exit_sessions: AtomicI64,
    pub flows: AtomicI64,
    pub probes_answered: AtomicU64,
    /// Bufferbloat tests (ADR 011): tickets first used, connections
    /// refused, and the load's bytes each way.
    pub doctor_tests: AtomicU64,
    pub doctor_refused: AtomicU64,
    pub doctor_bytes_sent: AtomicU64,
    pub doctor_bytes_received: AtomicU64,
    processing: Histogram,
    peers: Mutex<Vec<PeerLatency>>,
    regions: Mutex<Vec<RegionLatency>>,
}

/// An exit's round trip to the game regions it serves (their ping targets).
#[derive(Debug, Clone, PartialEq)]
pub struct RegionLatency {
    pub game: String,
    pub region: String,
    pub rtt_ms: Option<f64>,
    pub loss_pct: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PeerLatency {
    pub peer: String,
    pub rtt_ms: Option<f64>,
    pub loss_pct: f64,
}

#[derive(Debug, Default)]
struct Histogram {
    buckets: [AtomicU64; BUCKETS.len()],
    count: AtomicU64,
    sum_nanos: AtomicU64,
}

impl Metrics {
    pub fn count_drop(&self, reason: DropReason) {
        self.drops[reason as usize].fetch_add(1, Ordering::Relaxed);
    }

    pub fn drops(&self, reason: DropReason) -> u64 {
        self.drops[reason as usize].load(Ordering::Relaxed)
    }

    /// Time spent handling one frame, from receipt to send.
    pub fn record_processing(&self, elapsed: Duration) {
        let secs = elapsed.as_secs_f64();
        for (bucket, bound) in self.processing.buckets.iter().zip(BUCKETS) {
            if secs <= bound {
                bucket.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.processing.count.fetch_add(1, Ordering::Relaxed);
        self.processing
            .sum_nanos
            .fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn set_peer_latency(&self, peers: Vec<PeerLatency>) {
        if let Ok(mut current) = self.peers.lock() {
            *current = peers;
        }
    }

    pub fn peer_latency(&self) -> Vec<PeerLatency> {
        self.peers.lock().map(|p| p.clone()).unwrap_or_default()
    }

    pub fn set_region_latency(&self, regions: Vec<RegionLatency>) {
        if let Ok(mut current) = self.regions.lock() {
            *current = regions;
        }
    }

    pub fn region_latency(&self) -> Vec<RegionLatency> {
        self.regions.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// DropReason counts with at least one drop, for the periodic log summary.
    pub fn drop_summary(&self) -> Vec<(&'static str, u64)> {
        DropReason::ALL
            .iter()
            .map(|d| (d.label(), self.drops(*d)))
            .filter(|(_, n)| *n > 0)
            .collect()
    }

    /// The Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let gauge = |a: &AtomicI64| a.load(Ordering::Relaxed);

        let _ = writeln!(
            out,
            "# HELP pingpath_relay_drops_total Frames or packets not forwarded, by reason."
        );
        let _ = writeln!(out, "# TYPE pingpath_relay_drops_total counter");
        for reason in DropReason::ALL {
            let _ = writeln!(
                out,
                "pingpath_relay_drops_total{{reason=\"{}\"}} {}",
                reason.label(),
                self.drops(reason)
            );
        }
        for (name, help, value) in [
            (
                "frames_received_total",
                "Frames received.",
                load(&self.frames_received),
            ),
            (
                "frames_forwarded_total",
                "Frames sent on.",
                load(&self.frames_forwarded),
            ),
            (
                "duplicated_frames_total",
                "Second copies of frames sent to a client's second path.",
                load(&self.duplicated_frames),
            ),
            (
                "duplicated_bytes_total",
                "Bytes in second copies sent to a client's second path.",
                load(&self.duplicated_bytes),
            ),
            (
                "doctor_tests_total",
                "Bufferbloat tests started (doctor tickets first used).",
                load(&self.doctor_tests),
            ),
            (
                "doctor_refused_total",
                "Doctor listener connections refused: bad requests, invalid tickets, limits.",
                load(&self.doctor_refused),
            ),
            (
                "doctor_bytes_sent_total",
                "Bytes the doctor listener sent as download load.",
                load(&self.doctor_bytes_sent),
            ),
            (
                "doctor_bytes_received_total",
                "Bytes the doctor listener took as upload load.",
                load(&self.doctor_bytes_received),
            ),
            (
                "bytes_received_total",
                "Bytes received in frames.",
                load(&self.bytes_received),
            ),
            (
                "bytes_forwarded_total",
                "Bytes sent in frames.",
                load(&self.bytes_forwarded),
            ),
            (
                "sessions_started_total",
                "Sessions accepted.",
                load(&self.sessions_started),
            ),
            (
                "sessions_ended_total",
                "Sessions expired or removed.",
                load(&self.sessions_ended),
            ),
            (
                "probes_answered_total",
                "PROBE frames answered.",
                load(&self.probes_answered),
            ),
        ] {
            let _ = writeln!(
                out,
                "# HELP pingpath_relay_{name} {help}\n# TYPE pingpath_relay_{name} counter"
            );
            let _ = writeln!(out, "pingpath_relay_{name} {value}");
        }
        for (name, help, value) in [
            (
                "entry_sessions",
                "Sessions this relay forwards as an entry.",
                gauge(&self.entry_sessions),
            ),
            (
                "exit_sessions",
                "Sessions this relay terminates as an exit.",
                gauge(&self.exit_sessions),
            ),
            (
                "flows",
                "Open UDP flows to game servers.",
                gauge(&self.flows),
            ),
        ] {
            let _ = writeln!(
                out,
                "# HELP pingpath_relay_{name} {help}\n# TYPE pingpath_relay_{name} gauge"
            );
            let _ = writeln!(out, "pingpath_relay_{name} {value}");
        }

        let h = &self.processing;
        let _ = writeln!(
            out,
            "# HELP pingpath_relay_processing_seconds Time from receiving a frame to sending it on."
        );
        let _ = writeln!(out, "# TYPE pingpath_relay_processing_seconds histogram");
        for (bucket, bound) in h.buckets.iter().zip(BUCKETS) {
            let _ = writeln!(
                out,
                "pingpath_relay_processing_seconds_bucket{{le=\"{bound}\"}} {}",
                load(bucket)
            );
        }
        let count = load(&h.count);
        let _ = writeln!(
            out,
            "pingpath_relay_processing_seconds_bucket{{le=\"+Inf\"}} {count}"
        );
        let _ = writeln!(
            out,
            "pingpath_relay_processing_seconds_sum {}",
            load(&h.sum_nanos) as f64 / 1e9
        );
        let _ = writeln!(out, "pingpath_relay_processing_seconds_count {count}");

        let peers = self.peer_latency();
        if !peers.is_empty() {
            let _ = writeln!(
                out,
                "# HELP pingpath_relay_peer_rtt_ms Round trip to other relays.\n# TYPE pingpath_relay_peer_rtt_ms gauge"
            );
            for p in &peers {
                if let Some(rtt) = p.rtt_ms {
                    let _ = writeln!(
                        out,
                        "pingpath_relay_peer_rtt_ms{{peer=\"{}\"}} {rtt}",
                        p.peer
                    );
                }
            }
            let _ = writeln!(
                out,
                "# HELP pingpath_relay_peer_loss_pct Probe loss to other relays.\n# TYPE pingpath_relay_peer_loss_pct gauge"
            );
            for p in &peers {
                let _ = writeln!(
                    out,
                    "pingpath_relay_peer_loss_pct{{peer=\"{}\"}} {}",
                    p.peer, p.loss_pct
                );
            }
        }
        let regions = self.region_latency();
        if !regions.is_empty() {
            let _ = writeln!(
                out,
                "# HELP pingpath_relay_region_rtt_ms Round trip to the game regions this exit serves.\n# TYPE pingpath_relay_region_rtt_ms gauge"
            );
            for r in regions.iter().filter(|r| r.rtt_ms.is_some()) {
                let _ = writeln!(
                    out,
                    "pingpath_relay_region_rtt_ms{{game=\"{}\",region=\"{}\"}} {}",
                    r.game,
                    r.region,
                    r.rtt_ms.unwrap_or_default()
                );
            }
            let _ = writeln!(
                out,
                "# HELP pingpath_relay_region_loss_pct Probe loss to the game regions this exit serves.\n# TYPE pingpath_relay_region_loss_pct gauge"
            );
            for r in &regions {
                let _ = writeln!(
                    out,
                    "pingpath_relay_region_loss_pct{{game=\"{}\",region=\"{}\"}} {}",
                    r.game, r.region, r.loss_pct
                );
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_drop_reason_is_listed_once() {
        for (i, reason) in DropReason::ALL.iter().enumerate() {
            assert_eq!(*reason as usize, i, "{reason:?} out of order");
        }
    }

    #[test]
    fn renders_prometheus_text() {
        let m = Metrics::default();
        m.count_drop(DropReason::NotAllowlisted);
        m.count_drop(DropReason::NotAllowlisted);
        m.record_processing(Duration::from_micros(30));
        m.entry_sessions.fetch_add(2, Ordering::Relaxed);
        m.set_region_latency(vec![
            RegionLatency {
                game: "cs2".into(),
                region: "eu-central".into(),
                rtt_ms: Some(2.25),
                loss_pct: 0.0,
            },
            RegionLatency {
                game: "cs2".into(),
                region: "eu-north".into(),
                rtt_ms: None,
                loss_pct: 100.0,
            },
        ]);
        m.set_peer_latency(vec![PeerLatency {
            peer: "fra-1".into(),
            rtt_ms: Some(42.5),
            loss_pct: 0.0,
        }]);
        let text = m.render();
        assert!(
            text.contains("pingpath_relay_drops_total{reason=\"not_allowlisted\"} 2"),
            "{text}"
        );
        assert!(
            text.contains("pingpath_relay_processing_seconds_bucket{le=\"0.00005\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("pingpath_relay_processing_seconds_bucket{le=\"0.00001\"} 0"),
            "{text}"
        );
        assert!(text.contains("pingpath_relay_entry_sessions 2"), "{text}");
        assert!(
            text.contains("pingpath_relay_region_rtt_ms{game=\"cs2\",region=\"eu-central\"} 2.25"),
            "{text}"
        );
        assert!(!text.contains("pingpath_relay_region_rtt_ms{game=\"cs2\",region=\"eu-north\"}"));
        assert!(
            text.contains("pingpath_relay_region_loss_pct{game=\"cs2\",region=\"eu-north\"} 100"),
            "{text}"
        );
        assert!(
            text.contains("pingpath_relay_peer_rtt_ms{peer=\"fra-1\"} 42.5"),
            "{text}"
        );
        assert_eq!(m.drop_summary(), vec![("not_allowlisted", 2)]);
    }
}
