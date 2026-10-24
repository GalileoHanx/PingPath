use std::collections::HashMap;
use std::net::IpAddr;

use pingpath_crowd::{Chosen, Client, Direct, Method, ProbeDirect, Source};
use rusqlite::Connection;

use super::*;
use crate::config::CrowdLimits;
use geo::Place;

/// Games "cs2" (regions "eu" and "me") and relays 1 (entry) and 2 (exit);
/// the relays say the path from 1 through 2 to "eu" takes 60 ms.
struct Fake;

impl Registry for Fake {
    fn region(&self, game: &str, region: &str) -> Result<(), &'static str> {
        match (game, region) {
            ("cs2", "eu" | "me") => Ok(()),
            ("cs2", _) => Err("unknown_region"),
            _ => Err("unknown_game"),
        }
    }

    fn relay(&self, id: u32, exit: bool) -> bool {
        matches!((id, exit), (1, false) | (2, true))
    }

    fn path_floor_ms(
        &self,
        entry: Option<u32>,
        exit: u32,
        _game: &str,
        region: &str,
        _now: u64,
    ) -> Option<f64> {
        (entry == Some(1) && exit == 2 && region == "eu").then_some(60.0)
    }
}

/// Every network in Riyadh on one ISP.
struct Table(HashMap<NetworkPrefix, Place>);

impl Lookup for Table {
    fn place(&self, prefix: &NetworkPrefix) -> Place {
        self.0.get(prefix).cloned().unwrap_or_else(|| Place {
            asn: Some(64500),
            isp: Some("Example Telecom".into()),
            country: Some("SA".into()),
            city: Some("Riyadh".into()),
        })
    }
}

const NOW: u64 = 1_800_000_000;

fn crowd(dir: &tempfile::TempDir, limits: CrowdLimits) -> Crowd {
    let config = CrowdConfig {
        state_dir: dir.path().to_path_buf(),
        city_db: None,
        asn_db: None,
        excluded_asns: vec![],
        retention_days: 30,
        limits,
    };
    let (store, _) = Store::open(&dir.path().join("crowd.sqlite"), 30, 64).unwrap();
    let mut counters = Counters::default();
    counters.roll(NOW);
    let mut crowd = Crowd::assemble(
        &config,
        CrowdKey::from_master(&[5; 32]),
        Box::new(Table(HashMap::new())),
        store,
        counters,
    );
    // The tests' clock, not the wall's.
    crowd.counters.get_mut().unwrap().roll(NOW);
    crowd
}

fn id(n: u8) -> ContributorId {
    ContributorId::from_bytes([n; 16])
}

fn ip(text: &str) -> IpAddr {
    text.parse().unwrap()
}

fn report(contributor: u8, records: Vec<Record>) -> Report {
    Report {
        contributor: id(contributor),
        client: Client {
            source: Source::App,
            version: "0.1.0".into(),
        },
        records,
    }
}

fn direct(region: &str, rtt_ms: f64) -> Direct {
    Direct {
        region: region.into(),
        rtt_ms,
        loss_pct: 0.0,
        jitter_ms: 1.0,
    }
}

fn measurement() -> Record {
    Record::Measurement {
        game: "cs2".into(),
        regions: vec![direct("eu", 95.0), direct("me", 40.0)],
        chosen: Some(Chosen {
            region: "eu".into(),
            exit: 2,
            entry: Some(1),
        }),
    }
}

fn session(ping_ms: f64) -> Record {
    Record::Session {
        game: "cs2".into(),
        region: "eu".into(),
        exit: 2,
        entry: Some(1),
        direct_ms: Some(95.0),
        ping_ms,
        loss_pct: 0.2,
        jitter_ms: 1.5,
    }
}

/// A stored row: kind, region, direct, ping, exit and city.
type Stored = (
    String,
    String,
    Option<f64>,
    Option<f64>,
    Option<i64>,
    Option<String>,
);

fn rows(crowd: Crowd) -> Vec<Stored> {
    let path = crowd.database().to_path_buf();
    drop(crowd);
    let connection = Connection::open(path).unwrap();
    let mut statement = connection
        .prepare("SELECT kind, region, direct_ms, ping_ms, exit, city FROM records ORDER BY rowid")
        .unwrap();
    statement
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn records_are_stored_with_their_place_and_bad_ones_left_out() {
    let dir = tempfile::tempdir().unwrap();
    let crowd = crowd(&dir, CrowdLimits::default());
    let unknown_game = Record::Measurement {
        game: "valorant".into(),
        regions: vec![direct("eu", 50.0)],
        chosen: None,
    };
    let unknown_relay = Record::Session {
        game: "cs2".into(),
        region: "eu".into(),
        exit: 9,
        entry: None,
        direct_ms: None,
        ping_ms: 70.0,
        loss_pct: 0.0,
        jitter_ms: 1.0,
    };
    let accepted = crowd
        .ingest(
            &report(
                1,
                vec![
                    measurement(),
                    session(70.0),
                    unknown_game,
                    unknown_relay,
                    // Below what the relays themselves measure.
                    session(30.0),
                    session(-5.0),
                ],
            ),
            ip("198.51.100.77"),
            NOW,
            &Fake,
        )
        .unwrap();
    assert_eq!(accepted.stored, 2);
    let reasons: Vec<(usize, &str)> = accepted
        .ignored
        .iter()
        .map(|i| (i.record, i.reason.as_str()))
        .collect();
    assert_eq!(
        reasons,
        [
            (2, "unknown_game"),
            (3, "unknown_relay"),
            (4, "implausible"),
            (5, "out_of_range")
        ]
    );
    let stored = rows(crowd);
    assert_eq!(
        stored,
        [
            (
                "direct".into(),
                "eu".into(),
                Some(95.0),
                None,
                Some(2),
                Some("Riyadh".into())
            ),
            (
                "direct".into(),
                "me".into(),
                Some(40.0),
                None,
                None,
                Some("Riyadh".into())
            ),
            (
                "session".into(),
                "eu".into(),
                Some(95.0),
                Some(70.0),
                Some(2),
                Some("Riyadh".into())
            ),
        ]
    );
}

#[test]
fn probe_records_keep_how_they_were_measured() {
    let dir = tempfile::tempdir().unwrap();
    let crowd = crowd(&dir, CrowdLimits::default());
    let probe = Report {
        contributor: id(1),
        client: Client {
            source: Source::Probe,
            version: "0.1.0".into(),
        },
        records: vec![Record::Probe {
            game: "cs2".into(),
            regions: vec![ProbeDirect {
                region: "me".into(),
                rtt_ms: 41.0,
                loss_pct: 0.0,
                jitter_ms: 2.0,
                method: Method::Tcp,
            }],
        }],
    };
    assert_eq!(
        crowd
            .ingest(&probe, ip("198.51.100.77"), NOW, &Fake)
            .unwrap()
            .stored,
        1
    );
    let path = crowd.database().to_path_buf();
    drop(crowd);
    let (source, method): (String, String) = Connection::open(path)
        .unwrap()
        .query_row("SELECT source, method FROM records", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((source.as_str(), method.as_str()), ("probe", "tcp"));
}

#[test]
fn malformed_reports_are_refused_whole() {
    let dir = tempfile::tempdir().unwrap();
    let crowd = crowd(&dir, CrowdLimits::default());
    let refused = crowd
        .ingest(&report(1, vec![]), ip("198.51.100.77"), NOW, &Fake)
        .unwrap_err();
    assert_eq!((refused.status, refused.code), (400, "invalid_report"));
}

#[test]
fn a_network_and_a_person_each_have_an_hourly_share() {
    let dir = tempfile::tempdir().unwrap();
    let limits = CrowdLimits {
        reports_per_network_per_hour: 3,
        reports_per_contributor_per_hour: 2,
        ..CrowdLimits::default()
    };
    let crowd = crowd(&dir, limits);
    let send = |who: u8, from: &str, now: u64| {
        crowd.ingest(&report(who, vec![measurement()]), ip(from), now, &Fake)
    };
    assert!(send(1, "198.51.100.1", NOW).is_ok());
    assert!(send(1, "198.51.100.2", NOW).is_ok());
    let refused = send(1, "198.51.100.3", NOW).unwrap_err();
    assert_eq!((refused.status, refused.code), (429, "crowd_limit"));
    assert_eq!(refused.retry_after, Some(3600 - NOW % 3600));
    // Someone else on the same /24 takes the network's last report.
    assert!(send(2, "198.51.100.4", NOW).is_ok());
    assert_eq!(
        send(3, "198.51.100.5", NOW).unwrap_err().code,
        "crowd_limit"
    );
    // Another network is unaffected, and the next hour starts afresh.
    assert!(send(3, "203.0.113.5", NOW).is_ok());
    assert!(send(1, "198.51.100.1", NOW + 3600).is_ok());
}

#[test]
fn a_network_gets_only_so_many_new_contributors_a_day() {
    let dir = tempfile::tempdir().unwrap();
    let limits = CrowdLimits {
        new_contributors_per_network_per_day: 2,
        ..CrowdLimits::default()
    };
    let crowd = crowd(&dir, limits);
    let send =
        |who: u8, from: &str| crowd.ingest(&report(who, vec![measurement()]), ip(from), NOW, &Fake);
    assert!(send(1, "198.51.100.1").is_ok());
    assert!(send(2, "198.51.100.2").is_ok());
    let refused = send(3, "198.51.100.3").unwrap_err();
    assert_eq!(refused.code, "crowd_limit");
    assert_eq!(refused.retry_after, Some(86400 - NOW % 86400));
    // Someone already known still reports; the new one can from elsewhere.
    assert!(send(1, "198.51.100.1").is_ok());
    assert!(send(3, "203.0.113.3").is_ok());
}

#[test]
fn an_isp_and_the_whole_store_have_a_daily_budget() {
    let dir = tempfile::tempdir().unwrap();
    let limits = CrowdLimits {
        rows_per_asn_per_day: 5,
        ..CrowdLimits::default()
    };
    let crowd = crowd(&dir, limits);
    // A measurement of two regions is two rows.
    for who in 1..=2 {
        assert!(
            crowd
                .ingest(
                    &report(who, vec![measurement()]),
                    ip("198.51.100.1"),
                    NOW,
                    &Fake
                )
                .is_ok()
        );
    }
    let refused = crowd
        .ingest(
            &report(3, vec![measurement()]),
            ip("203.0.113.1"),
            NOW,
            &Fake,
        )
        .unwrap_err();
    assert_eq!(refused.code, "crowd_limit");
    // A report whose records were all left out takes no rows.
    let nothing = crowd
        .ingest(
            &report(3, vec![session(-1.0)]),
            ip("203.0.113.1"),
            NOW,
            &Fake,
        )
        .unwrap();
    assert_eq!(nothing.stored, 0);
}

#[tokio::test]
async fn forgetting_deletes_a_persons_records_within_a_limit() {
    let dir = tempfile::tempdir().unwrap();
    let limits = CrowdLimits {
        forgets_per_network_per_hour: 1,
        new_contributors_per_network_per_day: 1,
        ..CrowdLimits::default()
    };
    let crowd = crowd(&dir, limits);
    crowd
        .ingest(
            &report(1, vec![measurement(), session(70.0)]),
            ip("198.51.100.1"),
            NOW,
            &Fake,
        )
        .unwrap();
    assert_eq!(
        crowd.forget(&id(1), ip("198.51.100.9"), NOW).await.unwrap(),
        3
    );
    let refused = crowd
        .forget(&id(1), ip("198.51.100.9"), NOW)
        .await
        .unwrap_err();
    assert_eq!(refused.code, "crowd_limit");
    // The network used its one new contributor today on the forgotten one.
    assert_eq!(
        crowd
            .ingest(
                &report(2, vec![measurement()]),
                ip("198.51.100.1"),
                NOW,
                &Fake
            )
            .unwrap_err()
            .code,
        "crowd_limit"
    );
    assert!(rows(crowd).is_empty());
}

#[test]
fn dates_are_written_as_the_calendar_has_them() {
    assert_eq!(date(0), "1970-01-01");
    assert_eq!(date(1_767_225_600), "2026-01-01");
    assert_eq!(date(1_709_164_800), "2024-02-29");
}
