//! Control plane configuration, read from a TOML file (see
//! `control/control.example.toml`).

use std::collections::HashSet;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Entry,
    Exit,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The public API: session tokens for clients, latency reports from
    /// relays. Put a TLS-terminating reverse proxy in front of it.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Health and metrics; keep it private (firewalled or localhost).
    #[serde(default = "default_admin")]
    pub admin_listen: Option<SocketAddr>,
    /// Ed25519 seed (base64) that signs session tokens, from
    /// `pingpath-control keygen`. Its public key goes in every relay's
    /// `issuer_public_keys`.
    pub issuer_seed_file: PathBuf,
    /// Game definitions; the ones built into the binary when unset.
    #[serde(default)]
    pub games_dir: Option<PathBuf>,
    /// The header a reverse proxy puts the client's address in, e.g.
    /// `X-Forwarded-For`; its last entry is used (the one the proxy added).
    /// Leave unset when clients connect directly, or anyone could pick the
    /// address their rate limit is counted against.
    #[serde(default)]
    pub client_ip_header: Option<String>,
    #[serde(default)]
    pub sessions: SessionLimits,
    #[serde(default)]
    pub doctor: DoctorLimits,
    /// Crowd data (docs/decisions/012-crowd-data-and-dashboard.md); off
    /// when unset.
    #[serde(default)]
    pub crowd: Option<CrowdConfig>,
    /// Every relay, as listed for clients and allowed to report latency.
    pub relays: Vec<RelayEntry>,
}

/// `[crowd]`: where players' shared measurements are kept, and the limits
/// on what one network or person may send.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrowdConfig {
    /// Holds `crowd.sqlite` and its key, `crowd.key`. Back up neither:
    /// expired and deleted records must not come back.
    pub state_dir: PathBuf,
    /// DB-IP's "IP to City Lite" (MMDB), from `infra/update-geo.sh`.
    /// Unset: records have no country or city.
    #[serde(default)]
    pub city_db: Option<PathBuf>,
    /// DB-IP's "IP to ASN Lite" (MMDB). Unset: records have no ISP.
    #[serde(default)]
    pub asn_db: Option<PathBuf>,
    /// Hosting providers' ASNs, left out of the published tables.
    #[serde(default = "default_excluded_asns")]
    pub excluded_asns: Vec<u32>,
    /// Days records are kept.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    #[serde(default)]
    pub limits: CrowdLimits,
}

/// The major clouds, and providers relays commonly run on: players don't
/// play from them, and someone filling the tables with made-up reports
/// would.
fn default_excluded_asns() -> Vec<u32> {
    vec![
        16509, 14618, // Amazon
        15169, 396982, // Google
        8075,   // Microsoft
        13335,  // Cloudflare
        14061,  // DigitalOcean
        24940,  // Hetzner
        16276,  // OVH
        63949,  // Akamai (Linode)
        20473,  // Vultr
        45102,  // Alibaba
        31898,  // Oracle
    ]
}

fn default_retention_days() -> u32 {
    30
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CrowdLimits {
    /// Reports per source network (/24, or /48 for IPv6) per hour.
    pub reports_per_network_per_hour: u32,
    /// New contributors per source network per day.
    pub new_contributors_per_network_per_day: u32,
    /// Reports per contributor per hour.
    pub reports_per_contributor_per_hour: u32,
    /// Delete requests per source network per hour.
    pub forgets_per_network_per_hour: u32,
    /// Rows from one ISP per day.
    pub rows_per_asn_per_day: u64,
    /// Rows in all per day.
    pub rows_per_day: u64,
    /// Reports waiting for the database; more are turned away.
    pub queue: usize,
}

impl Default for CrowdLimits {
    fn default() -> Self {
        Self {
            reports_per_network_per_hour: 60,
            new_contributors_per_network_per_day: 10,
            reports_per_contributor_per_hour: 20,
            forgets_per_network_per_hour: 10,
            rows_per_asn_per_day: 20_000,
            rows_per_day: 500_000,
            queue: 256,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayEntry {
    /// Must match the relay's own `id`; tokens name exits by it.
    pub id: u32,
    pub name: String,
    /// Where clients and other relays send UDP.
    pub address: SocketAddr,
    pub roles: Vec<Role>,
    /// Exit role: the game regions it serves (the relay's `regions`).
    #[serde(default)]
    pub regions: Vec<String>,
    /// Exit role: its WireGuard public key (base64), printed by
    /// `pingpath-relay keygen`.
    #[serde(default)]
    pub public_key: Option<String>,
    /// SHA-256 (hex) of the relay's API key, printed by
    /// `pingpath-control api-key`. Without it the relay can't report latency.
    #[serde(default)]
    pub api_key_sha256: Option<String>,
    /// Cloud provider, so clients can prefer paths that cross providers.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    /// Where clients draw a bufferbloat test's load: the relay's public
    /// `[doctor] listen` address. Unset: no tests against this relay
    /// (docs/decisions/011-home-network-doctor.md).
    #[serde(default)]
    pub doctor: Option<SocketAddr>,
}

impl RelayEntry {
    pub fn has_role(&self, role: Role) -> bool {
        self.roles.contains(&role)
    }
}

/// How long sessions last and how many one network may hold
/// (docs/decisions/006-control-plane.md).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SessionLimits {
    /// Lifetime of each token; clients renew before it runs out.
    pub token_lifetime_secs: u64,
    /// No renewal goes past this long after the session started.
    pub max_lifetime_secs: u64,
    /// The shortest gap between two renewals of a session.
    pub min_renew_interval_secs: u64,
    /// New sessions per source network (/24, or /64 for IPv6) per minute.
    pub issue_per_minute_per_network: u32,
    /// Sessions one source network may hold at once.
    pub max_active_per_network: usize,
    /// Sessions in total.
    pub max_active: usize,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            token_lifetime_secs: 900,
            max_lifetime_secs: 12 * 3600,
            min_renew_interval_secs: 60,
            issue_per_minute_per_network: 10,
            max_active_per_network: 32,
            max_active: 100_000,
        }
    }
}

/// Doctor tickets: what one bufferbloat test may draw from a relay, and
/// how many tests one network may start
/// (docs/decisions/011-home-network-doctor.md).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DoctorLimits {
    /// How long a ticket lets a test start load.
    pub ticket_lifetime_secs: u64,
    /// Bytes a relay may send under one ticket, and take.
    pub max_bytes: u64,
    /// Tickets per source network per hour (a /24, or a /48 for IPv6:
    /// one home holds a /56 or a /48, so a /64 would be too fine)...
    pub per_network_per_hour: u32,
    /// ...and per day.
    pub per_network_per_day: u32,
    /// Tickets per relay per hour, all networks together, so tests can't
    /// keep a relay's uplink full whatever addresses ask.
    pub per_relay_per_hour: u32,
}

impl Default for DoctorLimits {
    fn default() -> Self {
        Self {
            ticket_lifetime_secs: 60,
            max_bytes: 250_000_000,
            per_network_per_hour: 4,
            per_network_per_day: 10,
            per_relay_per_hour: 120,
        }
    }
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}

fn default_admin() -> Option<SocketAddr> {
    Some(SocketAddr::from(([127, 0, 0, 1], 9101)))
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let config: Config =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Catches mistakes that would otherwise surface as refused sessions.
    pub fn validate(&self) -> Result<(), String> {
        let mut problems = Vec::new();
        if self.relays.is_empty() {
            problems.push("relays must list at least one relay".to_string());
        }
        let mut ids = HashSet::new();
        for relay in &self.relays {
            let name = &relay.name;
            if !ids.insert(relay.id) {
                problems.push(format!("relay id {} appears twice", relay.id));
            }
            if relay.roles.is_empty() {
                problems.push(format!(
                    "relay {name}: roles must include \"entry\", \"exit\" or both"
                ));
            }
            if relay.has_role(Role::Exit) {
                if relay.regions.is_empty() {
                    problems.push(format!("relay {name}: the exit role needs regions"));
                }
                match &relay.public_key {
                    None => problems.push(format!("relay {name}: the exit role needs public_key")),
                    Some(key) if pingpath_tunnel::keys::decode(key).is_err() => {
                        problems.push(format!(
                            "relay {name}: public_key is not a base64 32-byte key"
                        ));
                    }
                    Some(_) => {}
                }
            }
            let bad_hash = |h: &String| h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit());
            if relay.api_key_sha256.as_ref().is_some_and(bad_hash) {
                problems.push(format!(
                    "relay {name}: api_key_sha256 must be 64 hex digits"
                ));
            }
        }
        if !self.relays.iter().any(|r| r.has_role(Role::Exit)) {
            problems.push("at least one relay needs the exit role".to_string());
        }
        let s = &self.sessions;
        if !(60..=3600).contains(&s.token_lifetime_secs) {
            problems.push("sessions.token_lifetime_secs must be 60 to 3600".to_string());
        }
        if s.max_lifetime_secs < s.token_lifetime_secs {
            problems.push(
                "sessions.max_lifetime_secs must be at least token_lifetime_secs".to_string(),
            );
        }
        if s.min_renew_interval_secs >= s.token_lifetime_secs {
            problems.push(
                "sessions.min_renew_interval_secs must be less than token_lifetime_secs"
                    .to_string(),
            );
        }
        if s.issue_per_minute_per_network == 0 || s.max_active_per_network == 0 || s.max_active == 0
        {
            problems.push("session limits must be greater than zero".to_string());
        }
        let d = &self.doctor;
        if !(10..=600).contains(&d.ticket_lifetime_secs) {
            problems.push("doctor.ticket_lifetime_secs must be 10 to 600".to_string());
        }
        if d.max_bytes == 0
            || d.per_network_per_hour == 0
            || d.per_network_per_day == 0
            || d.per_relay_per_hour == 0
        {
            problems.push("doctor limits must be greater than zero".to_string());
        }
        if let Some(crowd) = &self.crowd {
            if !(1..=90).contains(&crowd.retention_days) {
                problems.push("crowd.retention_days must be 1 to 90".to_string());
            }
            let l = &crowd.limits;
            if l.reports_per_network_per_hour == 0
                || l.new_contributors_per_network_per_day == 0
                || l.reports_per_contributor_per_hour == 0
                || l.forgets_per_network_per_hour == 0
                || l.rows_per_asn_per_day == 0
                || l.rows_per_day == 0
                || l.queue == 0
            {
                problems.push("crowd limits must be greater than zero".to_string());
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, String> {
        let config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        config.validate().map(|()| config)
    }

    #[test]
    fn example_config_is_valid() {
        let config = parse(include_str!("../control.example.toml")).unwrap();
        assert_eq!(config.sessions.token_lifetime_secs, 900);
        assert!(config.relays.iter().any(|r| r.has_role(Role::Exit)));
        assert!(config.relays.iter().any(|r| r.has_role(Role::Entry)));
    }

    #[test]
    fn reports_every_mistake() {
        let err = parse(
            "issuer_seed_file = \"s\"\n[sessions]\ntoken_lifetime_secs = 10\n\
             [[relays]]\nid = 1\nname = \"a\"\naddress = \"192.0.2.1:51820\"\nroles = [\"exit\"]\napi_key_sha256 = \"zz\"\n\
             [[relays]]\nid = 1\nname = \"b\"\naddress = \"192.0.2.2:51820\"\nroles = [\"entry\"]\n",
        )
        .unwrap_err();
        for expected in [
            "appears twice",
            "needs regions",
            "needs public_key",
            "64 hex digits",
            "60 to 3600",
        ] {
            assert!(err.contains(expected), "{expected}: {err}");
        }
        assert!(parse("issuer_seed_file = \"s\"\nrelays = []\nbogus = 1\n").is_err());
    }
}
