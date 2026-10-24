//! Crowd data (docs/decisions/012-crowd-data-and-dashboard.md): the reports
//! of players who share their measurements, kept so that published figures
//! can be worked out from them. Optional: without a `[crowd]` section the
//! control plane runs as before.
//!
//! On the way in, a report's address becomes its /24 or /48 network
//! (`NetworkPrefix`) before anything else sees it; the network is looked up
//! in offline address files, tagged with a keyed hash, and dropped. What is
//! stored is the ISP, country and city, the tag, the person's keyed hash and
//! the record's numbers. Nothing here logs an address, a tag or an ID.

pub mod geo;
pub mod key;
pub mod prefix;
pub mod store;

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::net::IpAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pingpath_crowd::{Accepted, ContributorId, IGNORED_REASONS, Ignored, Record, Report};

use crate::config::CrowdConfig;
use crate::state::Refusal;
use geo::{Files, Lookup, Misses};
use key::{ContributorHash, CrowdKey, NetworkTag};
use prefix::NetworkPrefix;
use store::{Insert, Kind, Row, Store};

/// An optimized ping may sit this far below the relays' own figures for
/// the path (their measurements and the client's differ a little).
const PLAUSIBLE_SLACK_MS: f64 = 10.0;

/// What a report is checked against: the games and relays this control
/// plane knows (implemented by `ControlState`).
pub trait Registry {
    /// `Err("unknown_game" | "unknown_region")` unless the game lists the
    /// region.
    fn region(&self, game: &str, region: &str) -> Result<(), &'static str>;
    /// Whether `id` is a relay with that role.
    fn relay(&self, id: u32, exit: bool) -> bool;
    /// The least an optimized ping on this path can be: the relays' own
    /// entry-to-exit plus exit-to-region round trips; none when they
    /// haven't reported them.
    fn path_floor_ms(
        &self,
        entry: Option<u32>,
        exit: u32,
        game: &str,
        region: &str,
        now: u64,
    ) -> Option<f64>;
}

/// The crowd module, when `[crowd]` is set.
pub struct Crowd {
    config: CrowdConfig,
    key: CrowdKey,
    places: Box<dyn Lookup>,
    /// The address files' misses, when they are the lookup.
    misses: Option<Arc<Misses>>,
    store: Store,
    counters: Mutex<Counters>,
    stats: Stats,
}

/// Per-hour and per-day counts for the limits, cleared as each hour and
/// day begins (by the clock, as the session limits are).
#[derive(Default)]
pub(crate) struct Counters {
    hour: u64,
    day: u64,
    reports_per_network: HashMap<NetworkTag, u32>,
    reports_per_contributor: HashMap<ContributorHash, u32>,
    forgets_per_network: HashMap<NetworkTag, u32>,
    new_per_network: HashMap<NetworkTag, u32>,
    rows_per_asn: HashMap<u32, u64>,
    rows: u64,
    /// Everyone with records, for the new-contributor limit.
    known: HashSet<ContributorHash>,
}

impl Counters {
    fn roll(&mut self, now: u64) {
        let (hour, day) = (now / 3600, now / 86400);
        if hour != self.hour {
            self.hour = hour;
            self.reports_per_network.clear();
            self.reports_per_contributor.clear();
            self.forgets_per_network.clear();
        }
        if day != self.day {
            self.day = day;
            self.new_per_network.clear();
            self.rows_per_asn.clear();
            self.rows = 0;
        }
    }
}

/// Counted for `/metrics`.
#[derive(Default)]
struct Stats {
    reports: AtomicU64,
    records_stored: AtomicU64,
    ignored: Mutex<HashMap<&'static str, u64>>,
    forgets: AtomicU64,
}

fn until_next_hour(now: u64) -> u64 {
    3600 - now % 3600
}

fn until_next_day(now: u64) -> u64 {
    86400 - now % 86400
}

fn limit(message: &str, retry_after: u64) -> Refusal {
    let mut refusal = Refusal::new(429, "crowd_limit", message);
    refusal.retry_after = Some(retry_after.max(1));
    refusal
}

impl Crowd {
    /// Opens the key, the address files and the database, and logs what
    /// is on.
    pub fn open(config: &CrowdConfig) -> Result<Self, String> {
        std::fs::create_dir_all(&config.state_dir)
            .map_err(|e| format!("cannot make {}: {e}", config.state_dir.display()))?;
        let key = CrowdKey::load_or_create(&config.state_dir.join("crowd.key"))?;
        let places = Files::open(config.city_db.as_deref(), config.asn_db.as_deref());
        let misses = places.misses.clone();
        let (store, opened) = Store::open(
            &config.state_dir.join("crowd.sqlite"),
            config.retention_days,
            config.limits.queue,
        )?;
        tracing::info!(
            state_dir = %config.state_dir.display(),
            retention_days = config.retention_days,
            contributors = opened.known.len(),
            rows_today = opened.rows_today,
            "crowd data on"
        );
        let counters = Counters {
            known: opened.known,
            rows_per_asn: opened.rows_today_by_asn,
            rows: opened.rows_today,
            ..Counters::default()
        };
        let mut crowd = Self::assemble(config, key, Box::new(places), store, counters);
        crowd.misses = Some(misses);
        Ok(crowd)
    }

    pub(crate) fn assemble(
        config: &CrowdConfig,
        key: CrowdKey,
        places: Box<dyn Lookup>,
        store: Store,
        mut counters: Counters,
    ) -> Self {
        // The counts read from the database are today's.
        let now = now_unix();
        counters.hour = now / 3600;
        counters.day = now / 86400;
        Self {
            config: config.clone(),
            key,
            places,
            misses: None,
            store,
            counters: Mutex::new(counters),
            stats: Stats::default(),
        }
    }

    /// Where the database is, for tests.
    pub fn database(&self) -> &Path {
        &self.store.path
    }

    /// `POST /v1/crowd/reports`.
    pub fn ingest(
        &self,
        report: &Report,
        from: IpAddr,
        now: u64,
        registry: &impl Registry,
    ) -> Result<Accepted, Refusal> {
        if let Some(why) = report.malformed() {
            return Err(Refusal::new(400, "invalid_report", why));
        }
        // The address goes no further than this block.
        let (place, network) = {
            let prefix = NetworkPrefix::of(from);
            (self.places.place(&prefix), self.key.tag(&prefix))
        };
        let contributor = self.key.contributor(&report.contributor);

        let mut rows = Vec::new();
        let mut ignored = Vec::new();
        for (index, record) in report.records.iter().enumerate() {
            match rows_for(record, registry, now) {
                Ok(mut more) => rows.append(&mut more),
                Err(reason) => ignored.push((index, reason)),
            }
        }
        let stored = report.records.len() - ignored.len();
        let asn = place.asn.unwrap_or(0);

        {
            let limits = &self.config.limits;
            let mut counters = self.counters.lock().expect("crowd counters lock");
            counters.roll(now);
            let from_network = counters
                .reports_per_network
                .get(&network)
                .copied()
                .unwrap_or(0);
            if from_network >= limits.reports_per_network_per_hour {
                return Err(limit(
                    "this network sent its share of reports this hour",
                    until_next_hour(now),
                ));
            }
            let from_contributor = counters
                .reports_per_contributor
                .get(&contributor)
                .copied()
                .unwrap_or(0);
            if from_contributor >= limits.reports_per_contributor_per_hour {
                return Err(limit(
                    "you sent your share of reports this hour",
                    until_next_hour(now),
                ));
            }
            let new = !counters.known.contains(&contributor);
            if new
                && !rows.is_empty()
                && counters.new_per_network.get(&network).copied().unwrap_or(0)
                    >= limits.new_contributors_per_network_per_day
            {
                return Err(limit(
                    "this network has as many new contributors as it may have today",
                    until_next_day(now),
                ));
            }
            let adding = rows.len() as u64;
            if counters.rows_per_asn.get(&asn).copied().unwrap_or(0) + adding
                > limits.rows_per_asn_per_day
                || counters.rows + adding > limits.rows_per_day
            {
                return Err(limit(
                    "the crowd store took all it may take today",
                    until_next_day(now),
                ));
            }
            if !rows.is_empty() {
                let insert = Insert {
                    contributor,
                    network,
                    received: now - now % 3600,
                    source: report.client.source,
                    place,
                    rows,
                };
                if self.store.insert(insert).is_err() {
                    let mut refusal =
                        Refusal::new(503, "crowd_busy", "the crowd store is busy; try later");
                    refusal.retry_after = Some(60);
                    return Err(refusal);
                }
                if new {
                    *counters.new_per_network.entry(network).or_default() += 1;
                    counters.known.insert(contributor);
                }
                *counters.rows_per_asn.entry(asn).or_default() += adding;
                counters.rows += adding;
            }
            *counters.reports_per_network.entry(network).or_default() += 1;
            *counters
                .reports_per_contributor
                .entry(contributor)
                .or_default() += 1;
        }

        self.stats.reports.fetch_add(1, Ordering::Relaxed);
        self.stats
            .records_stored
            .fetch_add(stored as u64, Ordering::Relaxed);
        {
            let mut counted = self.stats.ignored.lock().expect("crowd stats lock");
            for (_, reason) in &ignored {
                *counted.entry(reason).or_default() += 1;
            }
        }
        tracing::info!(
            source = report.client.source.as_str(),
            version = %report.client.version,
            records = report.records.len(),
            stored,
            ignored = ignored.len(),
            "crowd report"
        );
        Ok(Accepted {
            stored,
            ignored: ignored
                .into_iter()
                .map(|(record, reason)| Ignored {
                    record,
                    reason: reason.into(),
                })
                .collect(),
        })
    }

    /// `DELETE /v1/crowd/contributor`: everything one person shared.
    pub async fn forget(
        &self,
        contributor: &ContributorId,
        from: IpAddr,
        now: u64,
    ) -> Result<u64, Refusal> {
        let network = self.key.tag(&NetworkPrefix::of(from));
        let hash = self.key.contributor(contributor);
        {
            let mut counters = self.counters.lock().expect("crowd counters lock");
            counters.roll(now);
            let count = counters.forgets_per_network.entry(network).or_default();
            if *count >= self.config.limits.forgets_per_network_per_hour {
                return Err(limit(
                    "this network asked to delete records too often this hour",
                    until_next_hour(now),
                ));
            }
            *count += 1;
        }
        let deleted = self.store.forget(hash).await.map_err(|e| {
            let mut refusal = Refusal::new(503, "crowd_busy", e);
            refusal.retry_after = Some(60);
            refusal
        })?;
        self.counters
            .lock()
            .expect("crowd counters lock")
            .known
            .remove(&hash);
        self.stats.forgets.fetch_add(1, Ordering::Relaxed);
        Ok(deleted)
    }

    /// The crowd's series for `/metrics`.
    pub fn render_metrics(&self, out: &mut String) {
        let mut counter = |name: &str, help: &str, value: u64| {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}"
            );
        };
        counter(
            "pingpath_control_crowd_reports_total",
            "Crowd reports accepted.",
            self.stats.reports.load(Ordering::Relaxed),
        );
        counter(
            "pingpath_control_crowd_records_stored_total",
            "Crowd records accepted.",
            self.stats.records_stored.load(Ordering::Relaxed),
        );
        let store = &self.store.stats;
        counter(
            "pingpath_control_crowd_rows_written_total",
            "Crowd rows written to the database.",
            store.rows_written.load(Ordering::Relaxed),
        );
        counter(
            "pingpath_control_crowd_rows_expired_total",
            "Crowd rows deleted past retention.",
            store.rows_expired.load(Ordering::Relaxed),
        );
        counter(
            "pingpath_control_crowd_rows_forgotten_total",
            "Crowd rows deleted at their contributor's request.",
            store.rows_forgotten.load(Ordering::Relaxed),
        );
        counter(
            "pingpath_control_crowd_forgets_total",
            "Requests to delete a contributor's records.",
            self.stats.forgets.load(Ordering::Relaxed),
        );
        counter(
            "pingpath_control_crowd_store_errors_total",
            "Crowd database errors.",
            store.errors.load(Ordering::Relaxed),
        );
        let _ = writeln!(
            out,
            "# HELP pingpath_control_crowd_records_ignored_total Crowd records left out, by reason.\n\
             # TYPE pingpath_control_crowd_records_ignored_total counter"
        );
        let ignored = self.stats.ignored.lock().expect("crowd stats lock");
        for reason in IGNORED_REASONS {
            let _ = writeln!(
                out,
                "pingpath_control_crowd_records_ignored_total{{reason=\"{reason}\"}} {}",
                ignored.get(reason).copied().unwrap_or(0)
            );
        }
        drop(ignored);
        if let Some(files) = &self.misses {
            let _ = writeln!(
                out,
                "# HELP pingpath_control_crowd_lookup_misses_total Networks an address file had nothing for.\n\
                 # TYPE pingpath_control_crowd_lookup_misses_total counter\n\
                 pingpath_control_crowd_lookup_misses_total{{file=\"city\"}} {}\n\
                 pingpath_control_crowd_lookup_misses_total{{file=\"asn\"}} {}",
                files.city.load(Ordering::Relaxed),
                files.asn.load(Ordering::Relaxed),
            );
        }
    }
}

/// A record's rows, or why it is left out.
fn rows_for(record: &Record, registry: &impl Registry, now: u64) -> Result<Vec<Row>, &'static str> {
    if let Some(problem) = record.problem() {
        return Err(problem);
    }
    let direct = |game: &str, region: &str, rtt_ms, loss_pct, jitter_ms| Row {
        kind: Kind::Direct,
        game: game.into(),
        region: region.into(),
        direct_ms: Some(rtt_ms),
        ping_ms: None,
        loss_pct,
        jitter_ms,
        entry: None,
        exit: None,
        method: None,
    };
    match record {
        Record::Measurement {
            game,
            regions,
            chosen,
        } => {
            for region in regions {
                registry.region(game, &region.region)?;
            }
            if let Some(chosen) = chosen {
                if !registry.relay(chosen.exit, true)
                    || chosen
                        .entry
                        .is_some_and(|entry| !registry.relay(entry, false))
                {
                    return Err("unknown_relay");
                }
            }
            Ok(regions
                .iter()
                .map(|r| {
                    let mut row = direct(game, &r.region, r.rtt_ms, r.loss_pct, r.jitter_ms);
                    if let Some(chosen) = chosen.as_ref().filter(|c| c.region == r.region) {
                        row.exit = Some(chosen.exit);
                        row.entry = chosen.entry;
                    }
                    row
                })
                .collect())
        }
        Record::Session {
            game,
            region,
            exit,
            entry,
            direct_ms,
            ping_ms,
            loss_pct,
            jitter_ms,
        } => {
            registry.region(game, region)?;
            if !registry.relay(*exit, true) || entry.is_some_and(|e| !registry.relay(e, false)) {
                return Err("unknown_relay");
            }
            if let Some(floor) = registry.path_floor_ms(*entry, *exit, game, region, now) {
                if *ping_ms + PLAUSIBLE_SLACK_MS < floor {
                    return Err("implausible");
                }
            }
            Ok(vec![Row {
                kind: Kind::Session,
                game: game.clone(),
                region: region.clone(),
                direct_ms: *direct_ms,
                ping_ms: Some(*ping_ms),
                loss_pct: *loss_pct,
                jitter_ms: *jitter_ms,
                entry: *entry,
                exit: Some(*exit),
                method: None,
            }])
        }
        Record::Probe { game, regions } => {
            for region in regions {
                registry.region(game, &region.region)?;
            }
            Ok(regions
                .iter()
                .map(|r| Row {
                    method: Some(r.method),
                    ..direct(game, &r.region, r.rtt_ms, r.loss_pct, r.jitter_ms)
                })
                .collect())
        }
    }
}

fn now_unix() -> u64 {
    pingpath_tunnel::token::now_unix()
}

/// `YYYY-MM-DD` for a Unix time, in UTC.
pub fn date(unix: u64) -> String {
    // Howard Hinnant's days-to-civil.
    let days = (unix / 86400) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests;
