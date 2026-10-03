//! What the control plane decides: which sessions to grant, renew or
//! refuse, and which relay latency reports to accept. Every method takes
//! the current time, so the rules can be tested without waiting.
//!
//! Sessions live in memory; after a restart clients simply ask for new
//! ones. A session's record holds the requester's network only as a keyed
//! hash, used to count sessions per network, and no log line ever holds
//! the requester's address (PLAN.md rule 9).

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::Mutex;

use pingpath_games::{GameDefinition, RoutingPolicy};
use pingpath_tunnel::frame::{SESSION_ID_LEN, SessionId, session_hex};
use pingpath_tunnel::keys;
use pingpath_tunnel::token::{CLOCK_SKEW_SECS, Claims, Issuer};
use ring::digest::{SHA256, digest};
use ring::hmac;
use serde::{Deserialize, Serialize};

use crate::config::{Config, RelayEntry, Role};
use crate::metrics::Metrics;

/// Latency samples older than this are left out of `/v1/latency`.
pub const LATENCY_MAX_AGE_SECS: u64 = 120;
/// Records are swept at most this often.
const PRUNE_INTERVAL_SECS: u64 = 10;
const MAX_SAMPLES_PER_REPORT: usize = 1024;

/// Why a request was refused: an HTTP status, a stable code for clients
/// and metrics, and a message for people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    /// Seconds to wait before trying again, for rate limits.
    pub retry_after: Option<u64>,
}

impl Refusal {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after: None,
        }
    }

    fn retry(mut self, after: u64) -> Self {
        self.retry_after = Some(after.max(1));
        self
    }
}

/// `POST /v1/sessions`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRequest {
    /// A WireGuard public key (base64) made for this session alone.
    pub client_public_key: String,
    pub game: String,
    pub region: String,
    /// The exit to use; when absent the control plane picks one serving
    /// the region.
    #[serde(default)]
    pub exit_id: Option<u32>,
}

/// `POST /v1/sessions/renew`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenewRequest {
    pub session_id: String,
    pub renew_secret: String,
}

/// A relay as clients see it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RelayView {
    pub id: u32,
    pub name: String,
    pub address: String,
    pub roles: Vec<Role>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

impl From<&RelayEntry> for RelayView {
    fn from(r: &RelayEntry) -> Self {
        Self {
            id: r.id,
            name: r.name.clone(),
            address: r.address.to_string(),
            roles: r.roles.clone(),
            regions: r.regions.clone(),
            public_key: r.public_key.clone(),
            provider: r.provider.clone(),
            location: r.location.clone(),
        }
    }
}

/// A granted or renewed session.
#[derive(Debug, Clone, Serialize)]
pub struct SessionGrant {
    pub session_id: String,
    /// The signed token (base64) to send in HELLO.
    pub token: String,
    pub issued_at: u64,
    pub expires_at: u64,
    /// Renewals stop at this time; ask for a new session after it.
    pub renew_until: u64,
    /// Proves ownership when renewing (base64). Only in the first answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renew_secret: Option<String>,
    pub game: String,
    pub region: String,
    pub exit: RelayView,
    /// Entry relays the client may reach the exit through; it may also
    /// send straight to the exit.
    pub entries: Vec<RelayView>,
}

/// One relay-to-relay measurement, as relays report it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LatencySample {
    pub peer_id: u32,
    /// Median round trip; absent when every probe was lost.
    pub rtt_ms: Option<f64>,
    pub loss_pct: f64,
}

/// `POST /v1/relay-latency`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LatencyReport {
    pub relay_id: u32,
    pub samples: Vec<LatencySample>,
}

/// A row of `GET /v1/latency`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LatencyRow {
    pub relay_id: u32,
    pub peer_id: u32,
    pub rtt_ms: Option<f64>,
    pub loss_pct: f64,
    pub age_secs: u64,
}

/// A source network, as a keyed hash (the key is random per process).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct NetworkTag([u8; 8]);

#[derive(Debug)]
struct Record {
    client_key: [u8; 32],
    exit_id: u32,
    game: String,
    region: String,
    started: u64,
    expires_at: u64,
    last_issued: u64,
    renew_hash: [u8; 32],
    network: NetworkTag,
}

#[derive(Debug, Default)]
struct Sessions {
    records: HashMap<SessionId, Record>,
    by_client_key: HashMap<[u8; 32], SessionId>,
    per_network: HashMap<NetworkTag, usize>,
    per_exit: HashMap<u32, usize>,
    /// Sessions issued per network in the current minute.
    issued: HashMap<NetworkTag, u32>,
    minute: u64,
    last_prune: u64,
}

impl Sessions {
    fn insert(&mut self, id: SessionId, record: Record) {
        *self.per_network.entry(record.network).or_default() += 1;
        *self.per_exit.entry(record.exit_id).or_default() += 1;
        self.by_client_key.insert(record.client_key, id);
        self.records.insert(id, record);
    }

    /// Forgets sessions whose last token can no longer be used.
    fn prune(&mut self, now: u64) {
        let ended: Vec<SessionId> = self
            .records
            .iter()
            .filter(|(_, r)| now >= r.expires_at.saturating_add(CLOCK_SKEW_SECS))
            .map(|(id, _)| *id)
            .collect();
        for id in ended {
            let record = self.records.remove(&id).expect("listed above");
            self.by_client_key.remove(&record.client_key);
            decrement(&mut self.per_network, record.network);
            decrement(&mut self.per_exit, record.exit_id);
        }
        self.last_prune = now;
    }
}

fn decrement<K: std::hash::Hash + Eq>(counts: &mut HashMap<K, usize>, key: K) {
    if let Some(count) = counts.get_mut(&key) {
        *count -= 1;
        if *count == 0 {
            counts.remove(&key);
        }
    }
}

pub struct ControlState {
    pub config: Config,
    issuer: Issuer,
    games: HashMap<String, GameDefinition>,
    relays: BTreeMap<u32, RelayEntry>,
    /// SHA-256 of each relay's API key, to its ID.
    api_keys: Vec<([u8; 32], u32)>,
    network_key: hmac::Key,
    sessions: Mutex<Sessions>,
    latency: Mutex<HashMap<(u32, u32), (LatencySample, u64)>>,
    pub metrics: Metrics,
}

impl ControlState {
    pub fn new(
        config: Config,
        games: Vec<GameDefinition>,
        issuer_seed: &[u8; 32],
    ) -> Result<Self, String> {
        let issuer = Issuer::from_seed(issuer_seed).map_err(|e| format!("issuer seed: {e}"))?;
        let api_keys = config
            .relays
            .iter()
            .filter_map(|r| r.api_key_sha256.as_deref().map(|h| (h, r.id)))
            .map(|(hash, id)| {
                unhex(hash)
                    .map(|h| (h, id))
                    .ok_or("api_key_sha256 must be 64 hex digits")
            })
            .collect::<Result<_, _>>()?;
        let random = keys::random_32().map_err(|e| e.to_string())?;
        Ok(Self {
            issuer,
            games: games.into_iter().map(|g| (g.id.clone(), g)).collect(),
            relays: config.relays.iter().map(|r| (r.id, r.clone())).collect(),
            api_keys,
            network_key: hmac::Key::new(hmac::HMAC_SHA256, &random),
            sessions: Mutex::new(Sessions::default()),
            latency: Mutex::new(HashMap::new()),
            metrics: Metrics::default(),
            config,
        })
    }

    /// The key relays put in `issuer_public_keys`.
    pub fn issuer_public_key(&self) -> [u8; 32] {
        self.issuer.public_key()
    }

    pub fn relays(&self) -> Vec<RelayView> {
        self.relays.values().map(RelayView::from).collect()
    }

    pub fn active_sessions(&self) -> usize {
        self.sessions.lock().expect("sessions lock").records.len()
    }

    /// Grants a session for one game, region and exit (PLAN.md rules 3, 7, 10).
    pub fn issue(
        &self,
        request: &SessionRequest,
        from: IpAddr,
        now: u64,
    ) -> Result<SessionGrant, Refusal> {
        let result = self.try_issue(request, from, now);
        match &result {
            Ok(grant) => {
                self.metrics.issued();
                tracing::info!(
                    session = %grant.session_id,
                    game = %grant.game,
                    region = %grant.region,
                    exit = grant.exit.id,
                    "session issued"
                );
            }
            Err(refusal) => {
                self.metrics.refused(refusal.code);
                tracing::info!(reason = refusal.code, game = %request.game, region = %request.region, "session refused");
            }
        }
        result
    }

    fn try_issue(
        &self,
        request: &SessionRequest,
        from: IpAddr,
        now: u64,
    ) -> Result<SessionGrant, Refusal> {
        let client_key = keys::decode(&request.client_public_key)
            .map_err(|e| Refusal::new(400, "bad_client_key", format!("client_public_key: {e}")))?;
        let game = self.games.get(&request.game).ok_or_else(|| {
            Refusal::new(404, "unknown_game", format!("no game \"{}\"", request.game))
        })?;
        if game.status.routing_policy() == RoutingPolicy::Refuse {
            // Rule 7: untested and unsupported games are never routed.
            return Err(Refusal::new(
                403,
                "game_not_routable",
                format!(
                    "{} is {} and is never routed",
                    game.name,
                    game.status.as_str()
                ),
            ));
        }
        if game.region(&request.region).is_none() {
            return Err(Refusal::new(
                404,
                "unknown_region",
                format!("{} has no region \"{}\"", game.name, request.region),
            ));
        }

        let network = self.network_tag(from);
        let mut sessions = self.sessions.lock().expect("sessions lock");
        if now >= sessions.last_prune + PRUNE_INTERVAL_SECS {
            sessions.prune(now);
        }
        let exit = self.choose_exit(request, &sessions)?;
        let limits = &self.config.sessions;
        if sessions.by_client_key.contains_key(&client_key) {
            return Err(Refusal::new(
                409,
                "client_key_in_use",
                "this WireGuard key already has a session; make a new key for each session",
            ));
        }
        if sessions.records.len() >= limits.max_active {
            return Err(Refusal::new(503, "at_capacity", "no capacity for new sessions").retry(60));
        }
        if sessions.per_network.get(&network).copied().unwrap_or(0) >= limits.max_active_per_network
        {
            return Err(Refusal::new(
                429,
                "network_limit",
                "too many sessions from this network at once",
            )
            .retry(limits.token_lifetime_secs));
        }
        let minute = now / 60;
        if minute != sessions.minute {
            sessions.minute = minute;
            sessions.issued.clear();
        }
        let issued = sessions.issued.entry(network).or_default();
        if *issued >= limits.issue_per_minute_per_network {
            return Err(Refusal::new(
                429,
                "rate_limited",
                "too many new sessions from this network",
            )
            .retry(60 - now % 60));
        }
        *issued += 1;

        let random = keys::random_32().map_err(|_| internal())?;
        let session_id: SessionId = random[..SESSION_ID_LEN].try_into().expect("8 bytes");
        if sessions.records.contains_key(&session_id) {
            return Err(internal()); // 2^-64; let the client retry
        }
        let renew_secret = keys::random_32().map_err(|_| internal())?;
        let claims = Claims {
            session_id,
            client_key,
            exit_id: exit.id,
            issued_at: now,
            expires_at: now + limits.token_lifetime_secs,
            game: game.id.clone(),
            region: request.region.clone(),
        };
        let token = self.issuer.sign(&claims).map_err(|_| internal())?;
        sessions.insert(
            session_id,
            Record {
                client_key,
                exit_id: exit.id,
                game: game.id.clone(),
                region: request.region.clone(),
                started: now,
                expires_at: claims.expires_at,
                last_issued: now,
                renew_hash: sha256(&renew_secret),
                network,
            },
        );
        Ok(self.grant(
            &claims,
            &token,
            now + limits.max_lifetime_secs,
            Some(&renew_secret),
            exit,
        ))
    }

    /// The requested exit, or the exit serving the region with the fewest
    /// sessions.
    fn choose_exit(
        &self,
        request: &SessionRequest,
        sessions: &Sessions,
    ) -> Result<&RelayEntry, Refusal> {
        let serves = |r: &RelayEntry| r.has_role(Role::Exit) && r.regions.contains(&request.region);
        match request.exit_id {
            Some(id) => {
                let exit = self
                    .relays
                    .get(&id)
                    .filter(|r| r.has_role(Role::Exit))
                    .ok_or_else(|| {
                        Refusal::new(404, "unknown_exit", format!("no exit relay {id}"))
                    })?;
                if serves(exit) {
                    Ok(exit)
                } else {
                    Err(Refusal::new(
                        409,
                        "exit_wrong_region",
                        format!(
                            "exit {} does not serve region \"{}\"",
                            exit.name, request.region
                        ),
                    ))
                }
            }
            None => self
                .relays
                .values()
                .filter(|r| serves(r))
                .min_by_key(|r| sessions.per_exit.get(&r.id).copied().unwrap_or(0))
                .ok_or_else(|| {
                    Refusal::new(
                        503,
                        "no_exit",
                        format!("no exit relay serves region \"{}\" yet", request.region),
                    )
                }),
        }
    }

    /// A new token for an existing session: same session ID, key, game,
    /// region and exit, so relays keep its flows (the server sees no change).
    pub fn renew(&self, request: &RenewRequest, now: u64) -> Result<SessionGrant, Refusal> {
        let result = self.try_renew(request, now);
        match &result {
            Ok(grant) => {
                self.metrics.renewed();
                tracing::info!(session = %grant.session_id, "session renewed");
            }
            Err(refusal) => {
                self.metrics.refused(refusal.code);
                tracing::info!(reason = refusal.code, "renewal refused");
            }
        }
        result
    }

    fn try_renew(&self, request: &RenewRequest, now: u64) -> Result<SessionGrant, Refusal> {
        let unknown = || Refusal::new(404, "unknown_session", "no such session, or it has ended");
        let session_id: SessionId = unhex(&request.session_id).ok_or_else(unknown)?;
        let secret = base64_decode(&request.renew_secret).ok_or_else(unknown)?;
        let mut sessions = self.sessions.lock().expect("sessions lock");
        if now >= sessions.last_prune + PRUNE_INTERVAL_SECS {
            sessions.prune(now);
        }
        let record = sessions.records.get_mut(&session_id).ok_or_else(unknown)?;
        let presented = sha256(&secret);
        if presented
            .iter()
            .zip(&record.renew_hash)
            .fold(0u8, |d, (a, b)| d | (a ^ b))
            != 0
        {
            return Err(unknown());
        }
        let limits = &self.config.sessions;
        let renew_until = record.started + limits.max_lifetime_secs;
        if now >= renew_until.saturating_sub(limits.min_renew_interval_secs) {
            return Err(Refusal::new(
                410,
                "session_ended",
                "the session has reached its maximum lifetime",
            ));
        }
        let wait = (record.last_issued + limits.min_renew_interval_secs).saturating_sub(now);
        if wait > 0 {
            return Err(Refusal::new(429, "renewal_too_soon", "renewed too recently").retry(wait));
        }
        if self
            .games
            .get(&record.game)
            .is_none_or(|g| g.status.routing_policy() == RoutingPolicy::Refuse)
        {
            return Err(Refusal::new(
                403,
                "game_not_routable",
                "the game is no longer routed",
            ));
        }
        let exit = self.relays.get(&record.exit_id).ok_or_else(unknown)?;
        let claims = Claims {
            session_id,
            client_key: record.client_key,
            exit_id: record.exit_id,
            issued_at: now,
            expires_at: (now + limits.token_lifetime_secs).min(renew_until),
            game: record.game.clone(),
            region: record.region.clone(),
        };
        let token = self.issuer.sign(&claims).map_err(|_| internal())?;
        record.expires_at = claims.expires_at;
        record.last_issued = now;
        Ok(self.grant(&claims, &token, renew_until, None, exit))
    }

    fn grant(
        &self,
        claims: &Claims,
        token: &[u8],
        renew_until: u64,
        renew_secret: Option<&[u8; 32]>,
        exit: &RelayEntry,
    ) -> SessionGrant {
        SessionGrant {
            session_id: session_hex(&claims.session_id),
            token: base64_encode(token),
            issued_at: claims.issued_at,
            expires_at: claims.expires_at,
            renew_until,
            renew_secret: renew_secret.map(keys::encode),
            game: claims.game.clone(),
            region: claims.region.clone(),
            exit: exit.into(),
            entries: self
                .relays
                .values()
                .filter(|r| r.has_role(Role::Entry))
                .map(RelayView::from)
                .collect(),
        }
    }

    /// Stores a relay's latency measurements, if `api_key` belongs to it.
    pub fn report_latency(
        &self,
        api_key: &str,
        report: &LatencyReport,
        now: u64,
    ) -> Result<(), Refusal> {
        let result = self.try_report_latency(api_key, report, now);
        match &result {
            Ok(()) => {
                self.metrics.latency_report();
                tracing::debug!(
                    relay = report.relay_id,
                    samples = report.samples.len(),
                    "latency report"
                );
            }
            Err(refusal) => {
                self.metrics.refused(refusal.code);
                tracing::warn!(
                    reason = refusal.code,
                    relay = report.relay_id,
                    "latency report refused"
                );
            }
        }
        result
    }

    fn try_report_latency(
        &self,
        api_key: &str,
        report: &LatencyReport,
        now: u64,
    ) -> Result<(), Refusal> {
        let presented = sha256(api_key.trim().as_bytes());
        let relay = self
            .api_keys
            .iter()
            .find(|(hash, _)| {
                hash.iter()
                    .zip(&presented)
                    .fold(0u8, |d, (a, b)| d | (a ^ b))
                    == 0
            })
            .map(|(_, id)| *id)
            .ok_or_else(|| Refusal::new(401, "unauthorized", "unknown API key"))?;
        if relay != report.relay_id {
            return Err(Refusal::new(
                403,
                "wrong_relay",
                "this API key belongs to another relay",
            ));
        }
        if report.samples.len() > MAX_SAMPLES_PER_REPORT {
            return Err(Refusal::new(400, "bad_report", "too many samples"));
        }
        for sample in &report.samples {
            let rtt_ok = sample
                .rtt_ms
                .is_none_or(|ms| ms.is_finite() && (0.0..60_000.0).contains(&ms));
            let loss_ok = sample.loss_pct.is_finite() && (0.0..=100.0).contains(&sample.loss_pct);
            if !rtt_ok
                || !loss_ok
                || sample.peer_id == relay
                || !self.relays.contains_key(&sample.peer_id)
            {
                return Err(Refusal::new(
                    400,
                    "bad_report",
                    format!("bad sample for peer {}", sample.peer_id),
                ));
            }
        }
        let mut latency = self.latency.lock().expect("latency lock");
        for sample in &report.samples {
            latency.insert((relay, sample.peer_id), (*sample, now));
        }
        Ok(())
    }

    /// The latest relay-to-relay measurements, oldest dropped.
    pub fn latency(&self, now: u64) -> Vec<LatencyRow> {
        let mut latency = self.latency.lock().expect("latency lock");
        latency.retain(|_, (_, at)| now.saturating_sub(*at) <= LATENCY_MAX_AGE_SECS);
        let mut rows: Vec<LatencyRow> = latency
            .iter()
            .map(|(&(relay_id, peer_id), (s, at))| LatencyRow {
                relay_id,
                peer_id,
                rtt_ms: s.rtt_ms,
                loss_pct: s.loss_pct,
                age_secs: now.saturating_sub(*at),
            })
            .collect();
        rows.sort_by_key(|r| (r.relay_id, r.peer_id));
        rows
    }

    /// /24 for IPv4 and /64 for IPv6, keyed so the table never holds them.
    fn network_tag(&self, ip: IpAddr) -> NetworkTag {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
            v4 => v4,
        };
        let prefix: Vec<u8> = match ip {
            IpAddr::V4(v4) => v4.octets()[..3].to_vec(),
            IpAddr::V6(v6) => v6.octets()[..8].to_vec(),
        };
        let tag = hmac::sign(&self.network_key, &prefix);
        NetworkTag(
            tag.as_ref()[..8]
                .try_into()
                .expect("HMAC-SHA256 is 32 bytes"),
        )
    }
}

fn internal() -> Refusal {
    Refusal::new(500, "internal", "internal error; try again")
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    digest(&SHA256, bytes)
        .as_ref()
        .try_into()
        .expect("SHA-256 is 32 bytes")
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decodes exactly `N` bytes of hex.
pub fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let text = text.trim();
    if text.len() != 2 * N || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// The SHA-256 (hex) of an API key, as the config stores it.
pub fn api_key_hash(key: &str) -> String {
    hex(&sha256(key.trim().as_bytes()))
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(text: &str) -> Option<[u8; 32]> {
    keys::decode(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SessionLimits;
    use pingpath_games::SupportStatus;
    use pingpath_tunnel::token::Verifier;

    const NOW: u64 = 1_800_000_000;
    const HOME: &str = "203.0.113.7";

    /// The built-in games, marked routable (they are all untested today),
    /// plus one that is not.
    fn games() -> Vec<GameDefinition> {
        let mut games = pingpath_games::embedded().unwrap();
        let mut untested = games[0].clone();
        for game in &mut games {
            game.status = SupportStatus::Supported;
        }
        untested.id = "untested".into();
        untested.status = SupportStatus::Untested;
        games.push(untested);
        games
    }

    fn state(limits: SessionLimits) -> (ControlState, [u8; 32]) {
        let exit_key = pingpath_tunnel::keys::WireGuardKeys::generate()
            .unwrap()
            .public_base64();
        let relay = |id: u32, roles: Vec<Role>, regions: &[&str]| RelayEntry {
            id,
            name: format!("relay-{id}"),
            address: format!("192.0.2.{id}:51820").parse().unwrap(),
            regions: regions.iter().map(|r| r.to_string()).collect(),
            public_key: roles.contains(&Role::Exit).then(|| exit_key.clone()),
            api_key_sha256: Some(api_key_hash(&format!("key-{id}"))),
            roles,
            provider: None,
            location: None,
        };
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            admin_listen: None,
            issuer_seed_file: "unused".into(),
            games_dir: None,
            client_ip_header: None,
            sessions: limits,
            relays: vec![
                relay(1, vec![Role::Exit], &["eu-central"]),
                relay(
                    2,
                    vec![Role::Exit, Role::Entry],
                    &["eu-central", "eu-north"],
                ),
                relay(3, vec![Role::Entry], &[]),
            ],
        };
        config.validate().unwrap();
        let seed = keys::random_32().unwrap();
        let state = ControlState::new(config, games(), &seed).unwrap();
        let public = state.issuer_public_key();
        (state, public)
    }

    fn request(game: &str, region: &str, exit_id: Option<u32>) -> SessionRequest {
        SessionRequest {
            client_public_key: pingpath_tunnel::keys::WireGuardKeys::generate()
                .unwrap()
                .public_base64(),
            game: game.into(),
            region: region.into(),
            exit_id,
        }
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn token(grant: &SessionGrant, public: [u8; 32], now: u64) -> Claims {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&grant.token)
            .unwrap();
        Verifier::new(vec![public]).verify(&bytes, now).unwrap()
    }

    #[test]
    fn issues_a_token_relays_accept() {
        let (state, public) = state(SessionLimits::default());
        let request = request("counter-strike-2", "eu-central", Some(1));
        let grant = state.issue(&request, ip(HOME), NOW).unwrap();
        let claims = token(&grant, public, NOW);
        assert_eq!(session_hex(&claims.session_id), grant.session_id);
        assert_eq!(keys::encode(&claims.client_key), request.client_public_key);
        assert_eq!(
            (claims.exit_id, claims.game.as_str(), claims.region.as_str()),
            (1, "counter-strike-2", "eu-central")
        );
        assert_eq!(claims.expires_at, NOW + 900);
        assert_eq!(grant.renew_until, NOW + 12 * 3600);
        assert!(grant.renew_secret.is_some());
        assert_eq!(grant.exit.public_key.as_deref().map(|k| k.len()), Some(44));
        assert_eq!(
            grant.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(state.active_sessions(), 1);
    }

    #[test]
    fn refuses_what_relays_would_refuse() {
        let (state, _) = state(SessionLimits::default());
        let code = |r: SessionRequest| state.issue(&r, ip(HOME), NOW).unwrap_err().code;
        assert_eq!(
            code(request("untested", "eu-central", None)),
            "game_not_routable"
        );
        assert_eq!(
            code(request("no-such-game", "eu-central", None)),
            "unknown_game"
        );
        assert_eq!(
            code(request("counter-strike-2", "mars", None)),
            "unknown_region"
        );
        assert_eq!(
            code(request("counter-strike-2", "eu-central", Some(9))),
            "unknown_exit"
        );
        assert_eq!(
            code(request("counter-strike-2", "eu-central", Some(3))),
            "unknown_exit"
        );
        assert_eq!(
            code(request("counter-strike-2", "eu-north", Some(1))),
            "exit_wrong_region"
        );
        let mut bad_key = request("counter-strike-2", "eu-central", None);
        bad_key.client_public_key = "not a key".into();
        assert_eq!(code(bad_key), "bad_client_key");
        assert_eq!(state.active_sessions(), 0);
        assert_eq!(state.metrics.refusals("game_not_routable"), 1);
    }

    #[test]
    fn picks_the_least_loaded_exit_serving_the_region() {
        let (state, _) = state(SessionLimits::default());
        let exits: Vec<u32> = (0..4)
            .map(|_| {
                state
                    .issue(
                        &request("counter-strike-2", "eu-central", None),
                        ip(HOME),
                        NOW,
                    )
                    .unwrap()
                    .exit
                    .id
            })
            .collect();
        assert_eq!(exits.iter().filter(|&&e| e == 1).count(), 2);
        assert_eq!(exits.iter().filter(|&&e| e == 2).count(), 2);
        let north = state
            .issue(
                &request("counter-strike-2", "eu-north", None),
                ip(HOME),
                NOW,
            )
            .unwrap();
        assert_eq!(north.exit.id, 2);
    }

    #[test]
    fn one_session_per_client_key() {
        let (state, _) = state(SessionLimits::default());
        let request = request("counter-strike-2", "eu-central", None);
        state.issue(&request, ip(HOME), NOW).unwrap();
        assert_eq!(
            state.issue(&request, ip(HOME), NOW).unwrap_err().code,
            "client_key_in_use"
        );
        // Free again once the session has ended.
        state
            .issue(&request, ip(HOME), NOW + 900 + CLOCK_SKEW_SECS)
            .unwrap();
    }

    #[test]
    fn limits_new_sessions_per_network_per_minute() {
        let (state, _) = state(SessionLimits {
            issue_per_minute_per_network: 3,
            ..Default::default()
        });
        let start = NOW - NOW % 60;
        for last in 1..=3 {
            state
                .issue(
                    &request("dota-2", "eu-central", None),
                    ip(&format!("203.0.113.{last}")),
                    start,
                )
                .unwrap();
        }
        let refusal = state
            .issue(
                &request("dota-2", "eu-central", None),
                ip("203.0.113.200"),
                start + 5,
            )
            .unwrap_err();
        assert_eq!(
            (refusal.code, refusal.retry_after),
            ("rate_limited", Some(55))
        );
        // Another /24 is counted separately, and the next minute starts afresh.
        state
            .issue(
                &request("dota-2", "eu-central", None),
                ip("203.0.114.1"),
                start + 5,
            )
            .unwrap();
        state
            .issue(
                &request("dota-2", "eu-central", None),
                ip("203.0.113.9"),
                start + 60,
            )
            .unwrap();
        // IPv6 counts per /64; IPv4-mapped addresses count as IPv4.
        state
            .issue(
                &request("dota-2", "eu-central", None),
                ip("::ffff:203.0.113.10"),
                start + 61,
            )
            .unwrap();
        state
            .issue(
                &request("dota-2", "eu-central", None),
                ip("::ffff:203.0.113.11"),
                start + 61,
            )
            .unwrap();
        let mapped = state.issue(
            &request("dota-2", "eu-central", None),
            ip("::ffff:203.0.113.12"),
            start + 61,
        );
        assert_eq!(mapped.unwrap_err().code, "rate_limited");
        for host in 1..=3 {
            state
                .issue(
                    &request("dota-2", "eu-central", None),
                    ip(&format!("2001:db8:0:1::{host}")),
                    start + 61,
                )
                .unwrap();
        }
        let v6 = state.issue(
            &request("dota-2", "eu-central", None),
            ip("2001:db8:0:1:ffff::1"),
            start + 61,
        );
        assert_eq!(v6.unwrap_err().code, "rate_limited");
    }

    #[test]
    fn caps_sessions_held_at_once() {
        let (state, _) = state(SessionLimits {
            max_active_per_network: 2,
            max_active: 3,
            ..Default::default()
        });
        for _ in 0..2 {
            state
                .issue(&request("dota-2", "eu-central", None), ip(HOME), NOW)
                .unwrap();
        }
        assert_eq!(
            state
                .issue(&request("dota-2", "eu-central", None), ip(HOME), NOW)
                .unwrap_err()
                .code,
            "network_limit"
        );
        state
            .issue(
                &request("dota-2", "eu-central", None),
                ip("198.51.100.1"),
                NOW,
            )
            .unwrap();
        assert_eq!(
            state
                .issue(&request("dota-2", "eu-central", None), ip("192.0.2.1"), NOW)
                .unwrap_err()
                .code,
            "at_capacity"
        );
        // Ended sessions stop counting.
        state
            .issue(&request("dota-2", "eu-central", None), ip(HOME), NOW + 960)
            .unwrap();
    }

    #[test]
    fn renewal_keeps_the_session_until_its_maximum_lifetime() {
        let (state, public) = state(SessionLimits {
            max_lifetime_secs: 3600,
            ..Default::default()
        });
        let grant = state
            .issue(
                &request("counter-strike-2", "eu-central", None),
                ip(HOME),
                NOW,
            )
            .unwrap();
        let first = token(&grant, public, NOW);
        let renew = RenewRequest {
            session_id: grant.session_id.clone(),
            renew_secret: grant.renew_secret.clone().unwrap(),
        };

        let refusal = state.renew(&renew, NOW + 30).unwrap_err();
        assert_eq!(
            (refusal.code, refusal.retry_after),
            ("renewal_too_soon", Some(30))
        );

        let renewed = state.renew(&renew, NOW + 600).unwrap();
        let second = token(&renewed, public, NOW + 600);
        assert_eq!(second.session_id, first.session_id);
        assert_eq!(
            (
                second.client_key,
                second.exit_id,
                &second.game,
                &second.region
            ),
            (first.client_key, first.exit_id, &first.game, &first.region)
        );
        assert_eq!(second.expires_at, NOW + 1500);
        assert!(
            renewed.renew_secret.is_none(),
            "the secret is only sent once"
        );

        // Renewing before each token runs out keeps the session going; the
        // last token stops at the maximum lifetime, and then renewals end.
        state.renew(&renew, NOW + 1400).unwrap();
        state.renew(&renew, NOW + 2200).unwrap();
        let last = state.renew(&renew, NOW + 3000).unwrap();
        assert_eq!(last.expires_at, NOW + 3600);
        assert_eq!(
            state.renew(&renew, NOW + 3550).unwrap_err().code,
            "session_ended"
        );
    }

    #[test]
    fn renewal_needs_the_secret_and_a_live_session() {
        let (state, _) = state(SessionLimits::default());
        let grant = state
            .issue(
                &request("counter-strike-2", "eu-central", None),
                ip(HOME),
                NOW,
            )
            .unwrap();
        let wrong = RenewRequest {
            session_id: grant.session_id.clone(),
            renew_secret: keys::encode(&[7; 32]),
        };
        assert_eq!(
            state.renew(&wrong, NOW + 300).unwrap_err().code,
            "unknown_session"
        );
        let garbage = RenewRequest {
            session_id: "xyz".into(),
            renew_secret: "!".into(),
        };
        assert_eq!(
            state.renew(&garbage, NOW + 300).unwrap_err().code,
            "unknown_session"
        );
        let late = RenewRequest {
            session_id: grant.session_id,
            renew_secret: grant.renew_secret.unwrap(),
        };
        assert_eq!(
            state
                .renew(&late, NOW + 900 + CLOCK_SKEW_SECS)
                .unwrap_err()
                .code,
            "unknown_session"
        );
    }

    #[test]
    fn latency_reports_need_the_relays_own_key() {
        let (state, _) = state(SessionLimits::default());
        let sample = |peer_id, rtt_ms| LatencySample {
            peer_id,
            rtt_ms,
            loss_pct: 0.0,
        };
        let report = LatencyReport {
            relay_id: 1,
            samples: vec![sample(2, Some(12.5)), sample(3, None)],
        };
        assert_eq!(
            state
                .report_latency("key-9", &report, NOW)
                .unwrap_err()
                .code,
            "unauthorized"
        );
        assert_eq!(
            state
                .report_latency("key-2", &report, NOW)
                .unwrap_err()
                .code,
            "wrong_relay"
        );
        let to_self = LatencyReport {
            relay_id: 1,
            samples: vec![sample(1, Some(1.0))],
        };
        assert_eq!(
            state
                .report_latency("key-1", &to_self, NOW)
                .unwrap_err()
                .code,
            "bad_report"
        );
        let negative = LatencyReport {
            relay_id: 1,
            samples: vec![sample(2, Some(-1.0))],
        };
        assert_eq!(
            state
                .report_latency("key-1", &negative, NOW)
                .unwrap_err()
                .code,
            "bad_report"
        );

        state.report_latency("key-1\n", &report, NOW).unwrap();
        let rows = state.latency(NOW + 5);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            LatencyRow {
                relay_id: 1,
                peer_id: 2,
                rtt_ms: Some(12.5),
                loss_pct: 0.0,
                age_secs: 5
            }
        );
        assert!(state.latency(NOW + LATENCY_MAX_AGE_SECS + 1).is_empty());
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(unhex::<2>(&hex(&[0xab, 0x01])), Some([0xab, 0x01]));
        assert_eq!(unhex::<2>("abc"), None);
        assert_eq!(unhex::<2>("zz00"), None);
        assert_eq!(
            api_key_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
