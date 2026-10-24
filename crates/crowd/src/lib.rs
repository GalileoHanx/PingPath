//! Crowd data (docs/decisions/012-crowd-data-and-dashboard.md): the records
//! a client or the probe sends when its player shares measurements, and the
//! checks both ends apply, so the two can't drift.
//!
//! A record holds numbers and identifiers only: game and region IDs, relay
//! IDs, round trips, loss and jitter. Never an address, a place or free
//! text; the server works out the ISP and city itself, from the truncated
//! address it receives the report from.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Records per report, at most.
pub const MAX_RECORDS: usize = 20;
/// Regions per record, at most (games list a handful).
pub const MAX_REGIONS: usize = 32;
/// Longest game or region ID.
pub const MAX_ID_LEN: usize = 64;
/// Longest version string.
pub const MAX_VERSION_LEN: usize = 32;
/// Round trips beyond this aren't measurements of a usable path.
pub const MAX_RTT_MS: f64 = 2000.0;
pub const MAX_JITTER_MS: f64 = 1000.0;

/// A person who shares: 128 random bits, made once and kept in their own
/// profile. It works like a password for their records, so it is never
/// shown, printed or logged: `Debug` hides it, and there is no `Display`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContributorId([u8; 16]);

impl ContributorId {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// The 32 lowercase hex digits it is sent and stored as.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 32 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let mut bytes = [0u8; 16];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Self(bytes))
    }
}

impl fmt::Debug for ContributorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ContributorId(hidden)")
    }
}

impl Serialize for ContributorId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ContributorId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::from_hex(&text)
            .ok_or_else(|| serde::de::Error::custom("contributor must be 32 hex digits"))
    }
}

/// What sent a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The Windows client (its service, for a player who shares).
    App,
    /// `pingpath-probe run --share`.
    Probe,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Probe => "probe",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Client {
    pub source: Source,
    pub version: String,
}

/// `POST /v1/crowd/reports`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub contributor: ContributorId,
    pub client: Client,
    pub records: Vec<Record>,
}

/// `DELETE /v1/crowd/contributor`: everything this person shared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forget {
    pub contributor: ContributorId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Record {
    /// After a measurement: every region's direct figures, and the path
    /// the client chose for the region it would use.
    Measurement {
        game: String,
        regions: Vec<Direct>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chosen: Option<Chosen>,
    },
    /// When an optimized session of at least a minute ends.
    Session {
        game: String,
        region: String,
        exit: u32,
        /// The session's first entry; none when it went straight to the
        /// exit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry: Option<u32>,
        /// The direct round trip measured before it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        direct_ms: Option<f64>,
        /// The session's median ping, as the client shows it: the tunnel's
        /// echo round trip plus the exit's round trip to the region.
        ping_ms: f64,
        /// Lost over sent, for the whole session.
        loss_pct: f64,
        /// The median of the session's jitter samples.
        jitter_ms: f64,
    },
    /// After `pingpath-probe run --share`: every region's direct figures.
    Probe {
        game: String,
        regions: Vec<ProbeDirect>,
    },
}

/// One region's direct figures, from the client's measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Direct {
    pub region: String,
    pub rtt_ms: f64,
    pub loss_pct: f64,
    pub jitter_ms: f64,
}

/// The path a measurement chose, for the region it would use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chosen {
    pub region: String,
    pub exit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<u32>,
}

/// How the probe measured a region's best target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Icmp,
    Tcp,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Icmp => "icmp",
            Self::Tcp => "tcp",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeDirect {
    pub region: String,
    pub rtt_ms: f64,
    pub loss_pct: f64,
    pub jitter_ms: f64,
    pub method: Method,
}

/// The answer to a report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Accepted {
    /// Records kept.
    pub stored: usize,
    /// Records left out, and why (`out_of_range`, `unknown_game`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored: Vec<Ignored>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ignored {
    /// Its position in the report.
    pub record: usize,
    pub reason: String,
}

/// The answer to a forget request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Forgotten {
    /// Records deleted.
    pub deleted: u64,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn in_range(value: f64, max: f64) -> bool {
    value.is_finite() && (0.0..=max).contains(&value)
}

/// Round trip, loss and jitter all plausible.
fn figures_ok(rtt_ms: f64, loss_pct: f64, jitter_ms: f64) -> bool {
    in_range(rtt_ms, MAX_RTT_MS) && in_range(loss_pct, 100.0) && in_range(jitter_ms, MAX_JITTER_MS)
}

impl Report {
    /// What makes the report as a whole unusable, if anything: it is
    /// refused rather than read record by record.
    pub fn malformed(&self) -> Option<&'static str> {
        if self.records.is_empty() {
            return Some("a report needs at least one record");
        }
        if self.records.len() > MAX_RECORDS {
            return Some("too many records");
        }
        let version = &self.client.version;
        if version.is_empty()
            || version.len() > MAX_VERSION_LEN
            || !version.bytes().all(|b| b.is_ascii_graphic())
        {
            return Some("client.version must be 1 to 32 printable characters");
        }
        if self.records.iter().any(|r| r.regions() > MAX_REGIONS) {
            return Some("too many regions in a record");
        }
        if self
            .records
            .iter()
            .any(|r| r.source() != self.client.source)
        {
            return Some("records don't match the client that sent them");
        }
        None
    }
}

impl Record {
    pub fn game(&self) -> &str {
        match self {
            Self::Measurement { game, .. }
            | Self::Session { game, .. }
            | Self::Probe { game, .. } => game,
        }
    }

    /// Which tool sends this kind of record.
    pub fn source(&self) -> Source {
        match self {
            Self::Measurement { .. } | Self::Session { .. } => Source::App,
            Self::Probe { .. } => Source::Probe,
        }
    }

    fn regions(&self) -> usize {
        match self {
            Self::Measurement { regions, .. } => regions.len(),
            Self::Probe { regions, .. } => regions.len(),
            Self::Session { .. } => 1,
        }
    }

    /// Why this record must be left out, judged on its own (the server
    /// also checks it against the games and relays it knows).
    pub fn problem(&self) -> Option<&'static str> {
        match self {
            Self::Measurement {
                game,
                regions,
                chosen,
            } => {
                if !valid_id(game) {
                    return Some("bad_id");
                }
                if regions.is_empty() {
                    return Some("no_regions");
                }
                if regions.iter().any(|r| !valid_id(&r.region)) {
                    return Some("bad_id");
                }
                if regions
                    .iter()
                    .any(|r| !figures_ok(r.rtt_ms, r.loss_pct, r.jitter_ms))
                {
                    return Some("out_of_range");
                }
                if has_duplicates(regions.iter().map(|r| r.region.as_str())) {
                    return Some("duplicate_region");
                }
                if let Some(chosen) = chosen {
                    if !regions.iter().any(|r| r.region == chosen.region) {
                        return Some("chosen_not_measured");
                    }
                }
                None
            }
            Self::Session {
                game,
                region,
                direct_ms,
                ping_ms,
                loss_pct,
                jitter_ms,
                ..
            } => {
                if !valid_id(game) || !valid_id(region) {
                    return Some("bad_id");
                }
                if !figures_ok(*ping_ms, *loss_pct, *jitter_ms)
                    || direct_ms.is_some_and(|d| !in_range(d, MAX_RTT_MS))
                {
                    return Some("out_of_range");
                }
                None
            }
            Self::Probe { game, regions } => {
                if !valid_id(game) {
                    return Some("bad_id");
                }
                if regions.is_empty() {
                    return Some("no_regions");
                }
                if regions.iter().any(|r| !valid_id(&r.region)) {
                    return Some("bad_id");
                }
                if regions
                    .iter()
                    .any(|r| !figures_ok(r.rtt_ms, r.loss_pct, r.jitter_ms))
                {
                    return Some("out_of_range");
                }
                if has_duplicates(regions.iter().map(|r| r.region.as_str())) {
                    return Some("duplicate_region");
                }
                None
            }
        }
    }
}

fn has_duplicates<'a>(items: impl Iterator<Item = &'a str>) -> bool {
    let mut seen = std::collections::HashSet::new();
    items.into_iter().any(|item| !seen.insert(item))
}

/// Every reason a record can be left out, the server's own included, so
/// each counter exists from the start.
pub const IGNORED_REASONS: &[&str] = &[
    "bad_id",
    "no_regions",
    "out_of_range",
    "duplicate_region",
    "chosen_not_measured",
    "unknown_game",
    "unknown_region",
    "unknown_relay",
    "implausible",
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id() -> ContributorId {
        ContributorId::from_bytes([0xab; 16])
    }

    fn session(ping_ms: f64) -> Record {
        Record::Session {
            game: "cs2".into(),
            region: "eu-central".into(),
            exit: 2,
            entry: Some(1),
            direct_ms: Some(90.0),
            ping_ms,
            loss_pct: 0.5,
            jitter_ms: 1.2,
        }
    }

    #[test]
    fn a_report_reads_as_the_adr_writes_it() {
        let text = json!({
            "contributor": "abababababababababababababababab",
            "client": {"source": "app", "version": "0.1.0"},
            "records": [
                {"kind": "measurement", "game": "cs2",
                 "regions": [{"region": "eu-central", "rtt_ms": 92.5, "loss_pct": 0.0, "jitter_ms": 1.5},
                             {"region": "me-south", "rtt_ms": 40.0, "loss_pct": 2.0, "jitter_ms": 3.0}],
                 "chosen": {"region": "eu-central", "exit": 2, "entry": 1}},
                {"kind": "session", "game": "cs2", "region": "eu-central", "exit": 2, "entry": 1,
                 "direct_ms": 92.5, "ping_ms": 70.0, "loss_pct": 0.1, "jitter_ms": 1.0}
            ]
        });
        let report: Report = serde_json::from_value(text.clone()).unwrap();
        assert_eq!(report.contributor, id());
        assert_eq!(report.records.len(), 2);
        assert_eq!(report.malformed(), None);
        assert!(report.records.iter().all(|r| r.problem().is_none()));
        assert_eq!(serde_json::to_value(&report).unwrap(), text);
    }

    #[test]
    fn the_contributor_id_never_shows_in_debug_output() {
        let report = Report {
            contributor: id(),
            client: Client {
                source: Source::App,
                version: "0.1.0".into(),
            },
            records: vec![session(70.0)],
        };
        let debug = format!("{report:?}");
        assert!(!debug.contains("abab"), "{debug}");
        assert!(debug.contains("hidden"));
        assert_eq!(ContributorId::from_hex(&id().to_hex()), Some(id()));
        assert_eq!(ContributorId::from_hex("not hex"), None);
        assert_eq!(ContributorId::from_hex(&"g".repeat(32)), None);
    }

    #[test]
    fn unknown_fields_and_addresses_are_refused() {
        // A record can't smuggle anything the schema doesn't list.
        let with_address = json!({
            "contributor": "abababababababababababababababab",
            "client": {"source": "app", "version": "0.1.0"},
            "records": [{"kind": "session", "game": "cs2", "region": "eu", "exit": 1,
                         "ping_ms": 50.0, "loss_pct": 0.0, "jitter_ms": 1.0,
                         "address": "203.0.113.7"}]
        });
        assert!(serde_json::from_value::<Report>(with_address).is_err());
    }

    #[test]
    fn whole_reports_are_refused_only_when_malformed() {
        let mut report = Report {
            contributor: id(),
            client: Client {
                source: Source::App,
                version: "0.1.0".into(),
            },
            records: vec![],
        };
        assert!(report.malformed().is_some());
        report.records = vec![session(70.0); MAX_RECORDS + 1];
        assert_eq!(report.malformed(), Some("too many records"));
        report.records = vec![session(70.0)];
        report.client.version = "a b".into();
        assert!(report.malformed().is_some());
        report.client.version = "0.1.0".into();
        report.client.source = Source::Probe;
        assert!(report.malformed().is_some(), "an app record from the probe");
    }

    #[test]
    fn records_out_of_range_are_left_out_one_by_one() {
        assert_eq!(session(70.0).problem(), None);
        assert_eq!(session(-1.0).problem(), Some("out_of_range"));
        assert_eq!(session(f64::NAN).problem(), Some("out_of_range"));
        assert_eq!(session(2500.0).problem(), Some("out_of_range"));
        let mut bad_id = session(70.0);
        if let Record::Session { game, .. } = &mut bad_id {
            *game = "CS2!".into();
        }
        assert_eq!(bad_id.problem(), Some("bad_id"));
        let measured = |regions: Vec<Direct>, chosen: Option<Chosen>| Record::Measurement {
            game: "cs2".into(),
            regions,
            chosen,
        };
        let direct = |region: &str| Direct {
            region: region.into(),
            rtt_ms: 50.0,
            loss_pct: 0.0,
            jitter_ms: 1.0,
        };
        assert_eq!(measured(vec![], None).problem(), Some("no_regions"));
        assert_eq!(
            measured(vec![direct("eu"), direct("eu")], None).problem(),
            Some("duplicate_region")
        );
        let chosen = Chosen {
            region: "us".into(),
            exit: 1,
            entry: None,
        };
        assert_eq!(
            measured(vec![direct("eu")], Some(chosen)).problem(),
            Some("chosen_not_measured")
        );
    }
}
