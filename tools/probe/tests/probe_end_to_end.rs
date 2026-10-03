//! End-to-end probe runs against this machine: ICMP and TCP targets on
//! loopback, a traced route, and a report that validates against the
//! published JSON Schema.
//!
//! ICMP needs no privileges on Windows. On Linux it needs ping sockets
//! (`net.ipv4.ping_group_range`); where they are off, the ICMP checks are
//! reported as skipped rather than failed, since that's a property of the
//! machine, not the code.

use std::io;
use std::net::{Ipv4Addr, TcpListener};
use std::time::Duration;

use pingpath_games::{
    AntiCheat, GameDefinition, PingTarget, PortRange, Ports, Region, SupportStatus,
};
use pingpath_probe::icmp::{Pinger, Reply};
use pingpath_probe::measure::SeriesOptions;
use pingpath_probe::probe::{self, ProbeOptions};
use pingpath_probe::report::SCHEMA;
use pingpath_probe::trace;
use pingpath_probe::verdict::VerdictKind;

const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;

fn quick(count: u32) -> SeriesOptions {
    SeriesOptions {
        count,
        interval: Duration::from_millis(10),
        timeout: Duration::from_millis(500),
    }
}

/// `None` (skip) when this Linux machine has ping sockets turned off.
fn pinger() -> Option<Pinger> {
    match Pinger::new() {
        Ok(p) => Some(p),
        Err(e) if !cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied => {
            eprintln!("skipping ICMP check: {e}");
            None
        }
        Err(e) => panic!("cannot create an ICMP pinger: {e}"),
    }
}

#[test]
fn icmp_echo_to_loopback() {
    let Some(mut pinger) = pinger() else { return };
    match pinger
        .send(LOOPBACK, 64, 1, Duration::from_secs(1))
        .unwrap()
    {
        Reply::Echo { from, rtt } => {
            assert_eq!(from, LOOPBACK);
            assert!(rtt < Duration::from_millis(500));
        }
        other => panic!("expected an echo reply, got {other:?}"),
    }
}

#[test]
fn trace_to_loopback_reaches_it_at_the_first_hop() {
    if pinger().is_none() {
        return;
    }
    let route = trace::trace(LOOPBACK, 5, &quick(3)).unwrap();
    assert!(route.reached);
    assert_eq!(route.hops.len(), 1);
    assert_eq!(route.hops[0].address, Some(LOOPBACK));
    assert_eq!(route.hops[0].stats.received, 3);
}

fn loopback_game(tcp_port: u16) -> GameDefinition {
    let mut ping_targets = vec![PingTarget::Tcp((LOOPBACK, tcp_port).into())];
    if pinger().is_some() {
        ping_targets.push(PingTarget::Icmp(LOOPBACK.into()));
    }
    GameDefinition {
        id: "loopback".into(),
        name: "Loopback test game".into(),
        status: SupportStatus::Untested,
        status_reason: None,
        report: None,
        anti_cheat: vec![AntiCheat::None],
        ports: Ports {
            udp: vec![PortRange {
                first: 7777,
                last: 7777,
            }],
            tcp: vec![],
        },
        sdr_app_id: None,
        regions: vec![Region {
            id: "here".into(),
            name: "This machine".into(),
            // Never routed: the probe only reads ping_targets.
            server_ranges: vec!["198.51.100.0/24".parse().unwrap()],
            ping_targets,
            sdr_pops: vec![],
            location: Some(pingpath_games::Location {
                lat: 24.71,
                lon: 46.68,
            }),
        }],
        notes: None,
    }
}

#[test]
fn probe_report_is_complete_and_matches_the_schema() {
    let listener = TcpListener::bind((LOOPBACK, 0)).unwrap();
    let game = loopback_game(listener.local_addr().unwrap().port());
    let options = ProbeOptions {
        series: quick(5),
        trace_probes: 2,
        max_hops: 3,
        lookups: false,
        origin: pingpath_probe::geo::city("Riyadh"),
        ..ProbeOptions::default()
    };

    let report = probe::run(&game, &options);

    let region = &report.regions[0];
    assert!(
        region
            .targets
            .iter()
            .all(|t| t.stats.as_ref().is_some_and(|s| s.received == 5)),
        "{region:#?}"
    );
    assert_eq!(region.floor_ms, Some(0.0));
    // A loopback round trip is far below any threshold.
    assert_eq!(
        region.verdict.kind,
        VerdictKind::NearOptimal,
        "{:#?}",
        region.verdict
    );
    if pinger().is_some() {
        assert!(
            region
                .trace
                .as_ref()
                .is_some_and(|t| t.reached && t.error.is_none()),
            "{:#?}",
            region.trace
        );
    }

    let schema: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(&json)
        .map(|e| format!("{}: {e}", e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "report doesn't match report.schema.json: {errors:#?}\n{}",
        report.to_json()
    );

    let markdown = report.to_markdown();
    assert!(
        markdown.contains("# PingPath probe: Loopback test game"),
        "{markdown}"
    );
    assert!(
        markdown.contains("Close to the best the distance allows"),
        "{markdown}"
    );
}

/// Needs the simulated network from tools/netsim/three-hop.sh; CI runs it as
/// `sudo ip netns exec ppclient <test binary> --ignored multi_hop`.
#[test]
#[ignore = "needs the network namespaces from tools/netsim/three-hop.sh"]
fn multi_hop_trace_names_every_router() {
    let server = Ipv4Addr::new(10, 3, 0, 2);
    let route = trace::trace(server, 8, &quick(3)).unwrap();
    let addresses: Vec<Option<Ipv4Addr>> = route.hops.iter().map(|h| h.address).collect();
    assert!(route.reached, "{route:#?}");
    assert_eq!(
        addresses,
        [
            Some(Ipv4Addr::new(10, 1, 0, 1)),
            Some(Ipv4Addr::new(10, 2, 0, 2)),
            Some(server)
        ],
        "{route:#?}"
    );
    assert!(
        route.hops.iter().all(|h| h.stats.received == 3),
        "{route:#?}"
    );
}
