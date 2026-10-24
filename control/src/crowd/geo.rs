//! City and ISP from offline address files (docs/decisions/012, "City and
//! ISP"): DB-IP's free "IP to City Lite" and "IP to ASN Lite", in the MaxMind
//! DB format, read into memory and read again when they change. Nothing is
//! asked of another company at run time (rule 8).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use maxminddb::{Reader, geoip2};

use super::prefix::NetworkPrefix;

/// How often a file's modification time is checked.
const CHECK_EVERY: Duration = Duration::from_secs(60);
/// A file older than this gets a warning: DB-IP publishes monthly.
const STALE_SECS: u64 = 60 * 86400;
/// Longest ISP or city name kept.
const MAX_NAME: usize = 100;

/// What the address files say about a network. Any part may be unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Place {
    pub asn: Option<u32>,
    pub isp: Option<String>,
    /// ISO 3166-1 alpha-2, uppercase.
    pub country: Option<String>,
    /// As the city file names it (English).
    pub city: Option<String>,
}

/// Looks networks up. Takes a `NetworkPrefix`, never an address.
pub trait Lookup: Send + Sync {
    fn place(&self, prefix: &NetworkPrefix) -> Place;
}

/// Lookups that missed, per file, for `/metrics`.
#[derive(Debug, Default)]
pub struct Misses {
    pub city: AtomicU64,
    pub asn: AtomicU64,
}

/// The two files, either of which may be unset.
pub struct Files {
    city: Option<Watched>,
    asn: Option<Watched>,
    pub misses: Arc<Misses>,
}

struct Watched {
    path: PathBuf,
    kind: &'static str,
    loaded: RwLock<Option<Loaded>>,
    checked: Mutex<Instant>,
}

struct Loaded {
    reader: Reader<Vec<u8>>,
    modified: Option<SystemTime>,
}

impl Files {
    pub fn open(city: Option<&Path>, asn: Option<&Path>) -> Self {
        let watch = |path: Option<&Path>, kind| {
            let Some(path) = path else {
                tracing::warn!(
                    file = kind,
                    "address file not set: crowd records have no {kind}"
                );
                return None;
            };
            let watched = Watched {
                path: path.to_path_buf(),
                kind,
                loaded: RwLock::new(None),
                checked: Mutex::new(Instant::now()),
            };
            watched.load();
            Some(watched)
        };
        Self {
            city: watch(city, "city"),
            asn: watch(asn, "asn"),
            misses: Arc::default(),
        }
    }
}

impl Watched {
    /// Reads the file (again); a file that won't read leaves the last one
    /// in use, or none.
    fn load(&self) {
        let modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        match Reader::open_readfile(&self.path) {
            Ok(reader) => {
                let built = reader.metadata().build_epoch;
                let age_days = now_unix().saturating_sub(built) / 86400;
                tracing::info!(
                    file = self.kind,
                    path = %self.path.display(),
                    database = %reader.metadata().database_type,
                    built = %super::date(built),
                    age_days,
                    "address file loaded"
                );
                if now_unix().saturating_sub(built) > STALE_SECS {
                    tracing::warn!(
                        file = self.kind,
                        age_days,
                        "address file is over 60 days old: run infra/update-geo.sh"
                    );
                }
                *self.loaded.write().expect("address file lock") =
                    Some(Loaded { reader, modified });
            }
            Err(e) => tracing::error!(
                file = self.kind,
                path = %self.path.display(),
                error = %e,
                "address file won't load"
            ),
        }
    }

    /// Reloads when the file changed, checking at most once a minute.
    fn refresh(&self) {
        {
            let mut checked = self.checked.lock().expect("address file lock");
            if checked.elapsed() < CHECK_EVERY {
                return;
            }
            *checked = Instant::now();
        }
        let modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        let current = self
            .loaded
            .read()
            .expect("address file lock")
            .as_ref()
            .and_then(|l| l.modified);
        if modified.is_some() && modified != current {
            self.load();
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A name as kept: trimmed, at most `MAX_NAME` characters, no control
/// characters; none if nothing is left.
fn name(text: &str) -> Option<String> {
    let clean: String = text
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect();
    (!clean.is_empty()).then_some(clean)
}

fn country(code: &str) -> Option<String> {
    (code.len() == 2 && code.bytes().all(|b| b.is_ascii_alphabetic()))
        .then(|| code.to_ascii_uppercase())
}

impl Lookup for Files {
    fn place(&self, prefix: &NetworkPrefix) -> Place {
        let network = prefix.network();
        let mut place = Place::default();
        if let Some(city) = &self.city {
            city.refresh();
            let loaded = city.loaded.read().expect("address file lock");
            let found = loaded.as_ref().and_then(|l| {
                l.reader
                    .lookup(network)
                    .ok()
                    .and_then(|r| r.decode::<geoip2::City>().ok().flatten())
                    .map(|c| {
                        (
                            c.country.iso_code.and_then(country),
                            c.city.names.english.and_then(name),
                        )
                    })
            });
            match found {
                Some((country, city)) => {
                    place.country = country;
                    place.city = city;
                }
                None => {
                    self.misses.city.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if let Some(asn) = &self.asn {
            asn.refresh();
            let loaded = asn.loaded.read().expect("address file lock");
            let found = loaded.as_ref().and_then(|l| {
                l.reader
                    .lookup(network)
                    .ok()
                    .and_then(|r| r.decode::<geoip2::Asn>().ok().flatten())
            });
            match found.and_then(|a| a.autonomous_system_number.map(|n| (n, a))) {
                Some((number, a)) => {
                    place.asn = Some(number);
                    place.isp = a.autonomous_system_organization.and_then(name);
                }
                None => {
                    self.misses.asn.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        place
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name)
    }

    fn files() -> Files {
        Files::open(
            Some(&fixture("geo-city.mmdb")),
            Some(&fixture("geo-asn.mmdb")),
        )
    }

    fn place(files: &Files, ip: &str) -> Place {
        files.place(&NetworkPrefix::of(ip.parse().unwrap()))
    }

    #[test]
    fn the_test_files_read_as_db_ip_files_do() {
        let files = files();
        assert_eq!(
            place(&files, "198.51.100.77"),
            Place {
                asn: Some(64500),
                isp: Some("Example Telecom".into()),
                country: Some("SA".into()),
                city: Some("Riyadh".into()),
            }
        );
        let jeddah = place(&files, "203.0.113.9");
        assert_eq!(jeddah.city.as_deref(), Some("Jeddah"));
        assert_eq!(jeddah.asn, Some(64501));
        // Every /24 of the benchmarking range is in Riyadh.
        assert_eq!(
            place(&files, "198.19.200.1").city.as_deref(),
            Some("Riyadh")
        );
        // No city: the country is still known.
        let kuwait = place(&files, "192.0.2.10");
        assert_eq!(kuwait.city, None);
        assert_eq!(kuwait.country.as_deref(), Some("KW"));
        let dubai = place(&files, "2001:db8:1234:5678::9");
        assert_eq!(dubai.city.as_deref(), Some("Dubai"));
        assert_eq!(dubai.asn, Some(64503));
        // Mapped addresses are their IPv4 address.
        assert_eq!(place(&files, "::ffff:198.51.100.77").asn, Some(64500));
    }

    #[test]
    fn unknown_networks_and_missing_files_give_unknown_places() {
        let files = files();
        assert_eq!(place(&files, "8.8.8.8"), Place::default());
        assert_eq!(files.misses.city.load(Ordering::Relaxed), 1);
        assert_eq!(files.misses.asn.load(Ordering::Relaxed), 1);
        let none = Files::open(None, Some(Path::new("/nonexistent/geo-asn.mmdb")));
        assert_eq!(place(&none, "198.51.100.77"), Place::default());
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(name("  Riyadh \n"), Some("Riyadh".into()));
        assert_eq!(name("\u{7}"), None);
        assert_eq!(name(&"x".repeat(500)).unwrap().len(), MAX_NAME);
        assert_eq!(country("sa"), Some("SA".into()));
        assert_eq!(country("SAU"), None);
    }
}
