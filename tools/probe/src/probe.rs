//! Runs a probe for one game: works out targets per region, measures them,
//! traces the route to the best one and writes the verdicts.

use std::net::{IpAddr, SocketAddr};
use std::thread;
use std::time::Duration;

use pingpath_games::{GameDefinition, PingTarget, Region};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::geo::{self, Origin};
use crate::icmp;
use crate::measure::{self, SeriesOptions};
use crate::netinfo::{self, Lookup};
use crate::report::{
    Connection, Game, HopReport, Method, Options, RegionReport, Report, SCHEMA_VERSION,
    TargetReport, TargetSource, Tool, TraceReport,
};
use crate::sdr::{self, SdrConfig};
use crate::trace;
use crate::udp;
use crate::verdict::{self, Inputs};

#[derive(Debug, Clone)]
pub struct ProbeOptions {
    pub series: SeriesOptions,
    pub trace: bool,
    pub trace_probes: u32,
    pub max_hops: u8,
    /// Only these region IDs; all regions when empty.
    pub regions: Vec<String>,
    pub origin: Option<Origin>,
    pub udp_echo: Option<SocketAddr>,
    /// Public-IP, ISP and per-hop network lookups (DNS).
    pub lookups: bool,
    pub relays_per_pop: usize,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            series: SeriesOptions {
                count: 20,
                interval: Duration::from_millis(200),
                timeout: Duration::from_millis(1000),
            },
            trace: true,
            trace_probes: 10,
            max_hops: 30,
            regions: Vec::new(),
            origin: None,
            udp_echo: None,
            lookups: true,
            relays_per_pop: 3,
        }
    }
}

/// One thing to measure in a region.
#[derive(Debug, Clone)]
pub struct TargetSpec {
    pub target: PingTarget,
    pub source: TargetSource,
    pub label: String,
}

pub fn run(game: &GameDefinition, options: &ProbeOptions) -> Report {
    tracing::info!(game = %game.id, "probe started");
    let lookup = Lookup::default();
    let mut notes = Vec::new();

    let connection = connection(options, &lookup);

    let regions: Vec<&Region> = game
        .regions
        .iter()
        .filter(|r| options.regions.is_empty() || options.regions.contains(&r.id))
        .collect();
    for wanted in &options.regions {
        if !game.regions.iter().any(|r| &r.id == wanted) {
            let known: Vec<&str> = game.regions.iter().map(|r| r.id.as_str()).collect();
            notes.push(format!(
                "{} has no region \"{wanted}\"; its regions are {}.",
                game.name,
                known.join(", ")
            ));
        }
    }

    let sdr = match game.sdr_app_id {
        Some(app_id) if regions.iter().any(|r| !r.sdr_pops.is_empty()) => Some(sdr::fetch(app_id)),
        _ => None,
    };
    if let Some(Err(e)) = &sdr {
        tracing::warn!(error = %e, "Valve relay list unavailable");
        notes.push(format!(
            "Valve's relay list could not be downloaded, so Valve relays were not measured: {e}"
        ));
    }

    let origin = connection.origin.clone();
    let region_reports: Vec<RegionReport> = thread::scope(|scope| {
        let handles: Vec<_> = regions
            .iter()
            .map(|region| {
                let (targets, target_notes) = targets(region, sdr.as_ref(), options.relays_per_pop);
                let (lookup, origin) = (&lookup, origin.as_ref());
                scope.spawn(move || {
                    let mut report = probe_region(region, targets, origin, options, lookup);
                    report.notes.splice(0..0, target_notes);
                    report
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });

    let udp_test =
        options
            .udp_echo
            .and_then(|endpoint| match udp::test(endpoint, &options.series) {
                Ok(test) => Some(test),
                Err(e) => {
                    tracing::warn!(error = %e, "UDP test failed");
                    notes.push(format!("UDP test failed: {e}"));
                    None
                }
            });
    if options.udp_echo.is_none() {
        notes.push(
            "UDP and WireGuard-throttling test skipped: pass --udp-echo HOST:PORT once a PingPath echo server is running."
                .to_string(),
        );
    }

    let report = Report {
        schema_version: SCHEMA_VERSION,
        tool: Tool {
            name: "pingpath-probe".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        },
        generated_at: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default(),
        game: Game {
            id: game.id.clone(),
            name: game.name.clone(),
            status: game.status.to_string(),
        },
        options: Options {
            count: options.series.count,
            interval_ms: options.series.interval.as_millis() as u64,
            timeout_ms: options.series.timeout.as_millis() as u64,
            trace: options.trace,
            trace_probes: options.trace_probes,
            max_hops: options.max_hops,
        },
        connection,
        regions: region_reports,
        udp_test,
        notes,
    };
    tracing::info!(game = %game.id, regions = report.regions.len(), "probe finished");
    report
}

fn connection(options: &ProbeOptions, lookup: &Lookup) -> Connection {
    let mut notes = Vec::new();
    let (mut public_ip, mut network) = (None, None);
    if options.lookups {
        match netinfo::public_ip() {
            Ok(ip) => {
                public_ip = Some(netinfo::redacted(IpAddr::V4(ip)));
                network = lookup.network(ip);
                if network.is_none() {
                    notes.push("Could not look up your ISP.".to_string());
                }
            }
            Err(e) => notes.push(format!("Could not find your public IP: {e}")),
        }
        tracing::info!(public_ip = ?public_ip, network = ?network, "connection");
    }
    let origin = options.origin.clone().or_else(|| {
        let guess = network
            .as_ref()
            .and_then(|n| n.country.as_deref())
            .and_then(geo::from_country);
        if guess.is_none() {
            notes.push(
                "Your location is unknown, so distance floors are skipped; pass --from <city>."
                    .to_string(),
            );
        }
        guess
    });
    Connection {
        public_ip,
        network,
        origin,
        notes,
    }
}

/// Targets for a region: Valve relays for its pops, then its ping targets;
/// plus notes on any relays that couldn't be found.
pub fn targets(
    region: &Region,
    sdr: Option<&Result<SdrConfig, String>>,
    relays_per_pop: usize,
) -> (Vec<TargetSpec>, Vec<String>) {
    let mut targets = Vec::new();
    let mut notes = Vec::new();
    for pop in &region.sdr_pops {
        let config = match sdr {
            Some(Ok(config)) => config,
            Some(Err(_)) => {
                notes.push(format!(
                    "Valve relays for {pop} not measured: the relay list could not be downloaded."
                ));
                continue;
            }
            None => continue,
        };
        match config.pops.get(pop) {
            Some(p) => {
                targets.extend(
                    p.relays
                        .iter()
                        .take(relays_per_pop)
                        .map(|relay| TargetSpec {
                            target: PingTarget::Icmp(IpAddr::V4(relay.address)),
                            source: TargetSource::SdrRelay,
                            label: format!("Valve relay, {pop}"),
                        }),
                )
            }
            None => {
                let available: Vec<String> = config
                    .pops
                    .iter()
                    .map(|(code, p)| {
                        format!("{code} ({})", p.description.as_deref().unwrap_or("?"))
                    })
                    .collect();
                tracing::warn!(region = %region.id, pop = %pop, available = %available.join(", "), "pop not in Valve's relay list");
                notes.push(format!(
                    "Valve's relay list has no \"{pop}\"; it has: {}.",
                    available.join(", ")
                ));
            }
        }
    }
    targets.extend(region.ping_targets.iter().map(|t| TargetSpec {
        target: *t,
        source: TargetSource::PingTarget,
        label: "ping target".to_string(),
    }));
    (targets, notes)
}

pub fn probe_region(
    region: &Region,
    targets: Vec<TargetSpec>,
    origin: Option<&Origin>,
    options: &ProbeOptions,
    lookup: &Lookup,
) -> RegionReport {
    tracing::info!(region = %region.id, targets = targets.len(), "measuring region");
    let mut notes = Vec::new();

    let (measurable, placeholders): (Vec<TargetSpec>, Vec<TargetSpec>) = targets
        .into_iter()
        .partition(|t| is_measurable(t.target.ip()));
    if !placeholders.is_empty() {
        notes.push(format!(
            "Skipped {} placeholder or private address(es) from the game definition.",
            placeholders.len()
        ));
    }
    if measurable.is_empty() {
        notes.push("Nothing to measure in this region yet.".to_string());
    }

    let target_reports: Vec<TargetReport> = thread::scope(|scope| {
        let handles: Vec<_> = measurable
            .iter()
            .map(|spec| scope.spawn(move || measure_target(spec, &options.series)))
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });

    let best = measurable
        .iter()
        .zip(&target_reports)
        .filter(|(_, r)| r.stats.as_ref().is_some_and(|s| s.received > 0))
        .min_by(|(_, a), (_, b)| avg(a).total_cmp(&avg(b)));
    let best_stats = best.and_then(|(_, r)| r.stats.clone()).or_else(|| {
        // Nothing answered: keep a 100%-loss result so the verdict says so.
        target_reports.iter().find_map(|r| r.stats.clone())
    });

    let trace = match (options.trace, best.map(|(spec, _)| spec.target.ip())) {
        (true, Some(IpAddr::V4(ip))) => Some(trace_route(ip, options, lookup)),
        (true, Some(IpAddr::V6(ip))) => Some(TraceReport {
            target: ip.to_string(),
            reached: false,
            hops: Vec::new(),
            error: Some("IPv6 routes can't be traced yet".into()),
        }),
        _ => None,
    };

    let (distance_km, floor_ms) = match (origin, region.location) {
        (Some(o), Some(l)) => {
            let km = geo::distance_km(o.lat, o.lon, l.lat, l.lon);
            (
                Some(km.round()),
                Some((geo::fiber_floor_ms(km) * 10.0).round() / 10.0),
            )
        }
        _ => (None, None),
    };
    let first_hop = trace
        .as_ref()
        .and_then(|t| t.hops.iter().find(|h| h.address.is_some()))
        .map(|h| &h.stats);
    let verdict = verdict::judge(&Inputs {
        best: best_stats.as_ref(),
        floor_ms,
        first_hop,
    });
    tracing::info!(region = %region.id, verdict = ?verdict.kind, summary = %verdict.summary, "region verdict");

    RegionReport {
        id: region.id.clone(),
        name: region.name.clone(),
        distance_km,
        floor_ms,
        best_target: best.map(|(spec, _)| spec.target.ip().to_string()),
        targets: target_reports,
        trace,
        verdict,
        notes,
    }
}

fn measure_target(spec: &TargetSpec, series: &SeriesOptions) -> TargetReport {
    let method = match spec.target {
        PingTarget::Icmp(_) => Method::Icmp,
        PingTarget::Tcp(_) => Method::Tcp,
    };
    let result = measure::target_series(&spec.target, series);
    let (stats, error) = match result {
        Ok(stats) => {
            tracing::info!(target = %spec.target, ?method, avg_ms = ?stats.avg_ms, loss_pct = stats.loss_pct, jitter_ms = ?stats.jitter_ms, "target measured");
            (Some(stats), None)
        }
        Err(e) => {
            tracing::warn!(target = %spec.target, ?method, error = %e, "target could not be measured");
            (None, Some(e.to_string()))
        }
    };
    TargetReport {
        address: spec.target.to_string(),
        method,
        source: spec.source,
        label: spec.label.clone(),
        stats,
        error,
    }
}

fn trace_route(target: std::net::Ipv4Addr, options: &ProbeOptions, lookup: &Lookup) -> TraceReport {
    if !icmp::TRACE_SUPPORTED {
        return TraceReport {
            target: target.to_string(),
            reached: false,
            hops: Vec::new(),
            error: Some("route tracing isn't supported on this OS".into()),
        };
    }
    let series = SeriesOptions {
        count: options.trace_probes,
        ..options.series
    };
    match trace::trace(target, options.max_hops, &series) {
        Ok(raw) => {
            let hops: Vec<HopReport> = thread::scope(|scope| {
                let handles: Vec<_> = raw
                    .hops
                    .iter()
                    .map(|hop| {
                        scope.spawn(move || HopReport {
                            ttl: hop.ttl,
                            address: hop.address.map(|a| a.to_string()),
                            network: hop
                                .address
                                .filter(|_| options.lookups)
                                .and_then(|a| lookup.network(a)),
                            stats: hop.stats.clone(),
                        })
                    })
                    .collect();
                handles.into_iter().filter_map(|h| h.join().ok()).collect()
            });
            for hop in &hops {
                tracing::info!(
                    target = %target,
                    ttl = hop.ttl,
                    address = hop.address.as_deref().unwrap_or("*"),
                    asn = ?hop.network.as_ref().map(|n| n.asn),
                    avg_ms = ?hop.stats.avg_ms,
                    loss_pct = hop.stats.loss_pct,
                    "hop"
                );
            }
            TraceReport {
                target: target.to_string(),
                reached: raw.reached,
                hops,
                error: None,
            }
        }
        Err(e) => {
            tracing::warn!(%target, error = %e, "route could not be traced");
            TraceReport {
                target: target.to_string(),
                reached: false,
                hops: Vec::new(),
                error: Some(e.to_string()),
            }
        }
    }
}

/// Documentation and private addresses in a definition are placeholders.
fn is_measurable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => netinfo::is_public(v4) || v4.is_loopback(),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback() || ((s[0] & 0xe000) == 0x2000 && !(s[0] == 0x2001 && s[1] == 0x0db8))
        }
    }
}

fn avg(report: &TargetReport) -> f64 {
    report
        .stats
        .as_ref()
        .and_then(|s| s.avg_ms)
        .unwrap_or(f64::INFINITY)
}
