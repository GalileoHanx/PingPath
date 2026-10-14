//! What the control plane decides: which sessions to grant, renew or
//! refuse, and which relay latency reports to accept. Every method takes
//! the current time, so the rules can be tested without waiting.
//!
//! Sessions live in memory. A restart forgets them, but renewal needs no
//! stored state: the renewal secret is derived from the issuer seed and
//! the session's claims, so a restarted control plane checks a renewal
//! against the token it carries and adopts the session again. A session's
//! record holds the requester's network only as a keyed
//! hash, used to count sessions per network, and no log line ever holds
//! the requester's address (PLAN.md rule 9).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;

use base64::Engine as _;
use pingpath_games::{GameDefinition, RoutingPolicy};
use pingpath_tunnel::frame::{SESSION_ID_LEN, SessionId, session_hex};
use pingpath_tunnel::keys;
use pingpath_tunnel::ticket::{self, Ticket};
use pingpath_tunnel::token::{CLOCK_SKEW_SECS, Claims, Issuer, Verifier};
use ring::digest::{SHA256, digest};
use ring::hmac;
use serde::{Deserialize, Serialize};

use crate::config::{Config, RelayEntry, Role};
use crate::metrics::Metrics;

/// Latency samples older than this are left out of `/v1/latency`.
pub const LATENCY_MAX_AGE_SECS: u64 = 120;
/// Rounds of each measurement pooled into its row: about a minute at the
/// relays' default interval, so loss is judged over 18 to 30 probes rather
/// than one round's three to five.
pub const WINDOW_ROUNDS: usize = 6;
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
    /// The client's digest of the region's routing definition
    /// (`GameDefinition::routing_digest`); a session is refused when it
    /// differs from this control plane's, which the relays share.
    #[serde(default)]
    pub definitions: Option<String>,
}

/// `POST /v1/sessions/renew`. The current token and `renew_until` let a
/// control plane that restarted (and forgot its sessions) check the
/// secret and adopt the session again.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenewRequest {
    pub session_id: String,
    pub renew_secret: String,
    /// The session's current token (base64).
    pub token: String,
    pub renew_until: u64,
}

/// `POST /v1/sessions/close`: the client is done with the session, so it
/// stops counting against its network's and its exit's limits.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseRequest {
    pub session_id: String,
    pub renew_secret: String,
}

/// A latency sample the control plane didn't store, and why: the rest of
/// the report is kept, so one stale definition on a relay costs only its
/// own rows.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Ignored {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    pub reason: &'static str,
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
    /// The relay's doctor listener, for bufferbloat tests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doctor: Option<String>,
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
            doctor: r.doctor.map(|a| a.to_string()),
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

/// What a client sends for a doctor ticket: the relay it will draw a
/// bufferbloat test's load from (docs/decisions/011-home-network-doctor.md).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctorTicketRequest {
    pub relay_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorTicketGrant {
    /// The signed ticket (base64) the relay's doctor listener wants.
    pub ticket: String,
    pub relay_id: u32,
    /// The relay's doctor listener.
    pub address: String,
    pub expires_at: u64,
    /// Bytes the relay sends under the ticket, and takes.
    pub max_bytes: u64,
}

/// Doctor tickets issued per network in the current hour and day, and per
/// relay in the current hour.
#[derive(Debug, Default)]
struct DoctorCounts {
    hour: u64,
    day: u64,
    per_hour: HashMap<NetworkTag, u32>,
    per_day: HashMap<NetworkTag, u32>,
    per_relay: HashMap<u32, u32>,
}

/// One relay-to-relay measurement, as relays report it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LatencySample {
    pub peer_id: u32,
    /// The round's mean round trip; absent when every probe was lost.
    pub rtt_ms: Option<f64>,
    pub loss_pct: f64,
    /// Probes sent and answered this round (0 and 0 from relays that
    /// don't say).
    #[serde(default)]
    pub sent: u32,
    #[serde(default)]
    pub received: u32,
}

/// `POST /v1/relay-latency`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LatencyReport {
    pub relay_id: u32,
    /// Exits: the WireGuard key they actually use (base64), checked
    /// against the registry so a wrong key shows up as such instead of as
    /// an exit that never answers.
    #[serde(default)]
    pub public_key: Option<String>,
    pub samples: Vec<LatencySample>,
    /// Exits only: round trips to the game regions they serve.
    #[serde(default)]
    pub regions: Vec<RegionSample>,
}

/// An exit's measurement of a game region it serves (its ping targets).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RegionSample {
    pub game: String,
    pub region: String,
    pub rtt_ms: Option<f64>,
    pub loss_pct: f64,
    #[serde(default)]
    pub sent: u32,
    #[serde(default)]
    pub received: u32,
}

/// A region row of `GET /v1/latency`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RegionRow {
    pub relay_id: u32,
    pub game: String,
    pub region: String,
    pub rtt_ms: Option<f64>,
    pub loss_pct: f64,
    /// Probes behind `loss_pct` (0 when the relay doesn't say).
    pub samples: u32,
    pub age_secs: u64,
}

/// A row of `GET /v1/latency`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LatencyRow {
    pub relay_id: u32,
    pub peer_id: u32,
    pub rtt_ms: Option<f64>,
    pub loss_pct: f64,
    /// Probes behind `loss_pct` (0 when the relay doesn't say).
    pub samples: u32,
    pub age_secs: u64,
}

/// One round of a measurement, as kept for its row.
#[derive(Debug, Clone, Copy)]
struct Round {
    rtt_ms: Option<f64>,
    loss_pct: f64,
    sent: u32,
    received: u32,
    at: u64,
}

/// The last rounds of one measurement, newest last.
#[derive(Debug, Default)]
struct Rounds(VecDeque<Round>);

impl Rounds {
    fn push(&mut self, round: Round) {
        self.0.push_back(round);
        while self.0.len() > WINDOW_ROUNDS {
            self.0.pop_front();
        }
    }

    /// Drops rounds too old to use; false when none are left.
    fn keep_fresh(&mut self, now: u64) -> bool {
        self.0
            .retain(|r| now.saturating_sub(r.at) <= LATENCY_MAX_AGE_SECS);
        !self.0.is_empty()
    }

    /// The row's figures: the median round trip of the rounds that were
    /// answered (none if the latest wasn't: the peer is down now), loss
    /// over every probe kept, how many that is, and the latest round's age.
    fn pooled(&self, now: u64) -> Option<(Option<f64>, f64, u32, u64)> {
        let latest = self.0.back()?;
        let rtt_ms = latest.rtt_ms.and_then(|_| {
            let mut rtts: Vec<f64> = self.0.iter().filter_map(|r| r.rtt_ms).collect();
            rtts.sort_by(f64::total_cmp);
            rtts.get(rtts.len() / 2).copied()
        });
        let counted = self.0.iter().all(|r| r.sent > 0);
        let (loss_pct, samples) = if counted {
            let sent: u32 = self.0.iter().map(|r| r.sent).sum();
            let received: u32 = self.0.iter().map(|r| r.received).sum();
            let lost = f64::from(sent.saturating_sub(received));
            ((lost / f64::from(sent) * 1000.0).round() / 10.0, sent)
        } else {
            (latest.loss_pct, 0)
        };
        Some((rtt_ms, loss_pct, samples, now.saturating_sub(latest.at)))
    }
}

/// (relay, game, region).
type RegionKey = (u32, String, String);

/// A source network, as a keyed hash (the key is random per process).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct NetworkTag([u8; 8]);

#[derive(Debug)]
struct Record {
    client_key: [u8; 32],
    exit_id: u32,
    game: String,
    region: String,
    renew_until: u64,
    expires_at: u64,
    last_issued: u64,
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
    /// Sessions their clients closed, until their last token stops
    /// verifying: a renewal of one must not adopt it again as if a restart
    /// had forgotten it, which would let a network hold any number of
    /// sessions while its count stays near zero.
    closed: HashMap<SessionId, u64>,
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
            self.remove(&id);
        }
        self.closed.retain(|_, until| now < *until);
        self.last_prune = now;
    }

    fn remove(&mut self, id: &SessionId) -> Option<Record> {
        let record = self.records.remove(id)?;
        self.by_client_key.remove(&record.client_key);
        decrement(&mut self.per_network, record.network);
        decrement(&mut self.per_exit, record.exit_id);
        Some(record)
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
    /// Derives each session's renewal secret. From the issuer seed, so it
    /// survives a restart and renewals keep working without stored state.
    renew_key: hmac::Key,
    sessions: Mutex<Sessions>,
    /// Exits whose reported key isn't the registry's: not offered as exits
    /// until they report the right one.
    key_mismatch: Mutex<HashSet<u32>>,
    latency: Mutex<HashMap<(u32, u32), Rounds>>,
    region_latency: Mutex<HashMap<RegionKey, Rounds>>,
    doctor: Mutex<DoctorCounts>,
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
            renew_key: hmac::Key::new(
                hmac::HMAC_SHA256,
                &sha256(&[&b"pingpath renew secret v1"[..], issuer_seed].concat()),
            ),
            sessions: Mutex::new(Sessions::default()),
            key_mismatch: Mutex::new(HashSet::new()),
            doctor: Mutex::new(DoctorCounts::default()),
            latency: Mutex::new(HashMap::new()),
            region_latency: Mutex::new(HashMap::new()),
            metrics: Metrics::default(),
            config,
        })
    }

    /// The key relays put in `issuer_public_keys`.
    pub fn issuer_public_key(&self) -> [u8; 32] {
        self.issuer.public_key()
    }

    /// The registry as clients see it. An exit whose key doesn't match is
    /// shown without one, so clients use it only as an entry.
    pub fn relays(&self) -> Vec<RelayView> {
        let mismatched = self.key_mismatch.lock().expect("key mismatch lock");
        self.relays
            .values()
            .map(|r| {
                let mut view = RelayView::from(r);
                if mismatched.contains(&r.id) {
                    view.public_key = None;
                }
                view
            })
            .collect()
    }

    fn usable_exit(&self, relay: &RelayEntry) -> bool {
        relay.has_role(Role::Exit)
            && !self
                .key_mismatch
                .lock()
                .expect("key mismatch lock")
                .contains(&relay.id)
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

    /// A doctor ticket for a relay with a doctor listener, within the
    /// per-network limits.
    pub fn issue_doctor_ticket(
        &self,
        request: &DoctorTicketRequest,
        from: IpAddr,
        now: u64,
    ) -> Result<DoctorTicketGrant, Refusal> {
        let result = self.try_issue_doctor_ticket(request, from, now);
        match &result {
            Ok(grant) => {
                self.metrics.doctor_ticket();
                tracing::info!(relay = grant.relay_id, "doctor ticket issued");
            }
            Err(refusal) => {
                self.metrics.refused(refusal.code);
                tracing::info!(
                    reason = refusal.code,
                    relay = request.relay_id,
                    "doctor ticket refused"
                );
            }
        }
        result
    }

    fn try_issue_doctor_ticket(
        &self,
        request: &DoctorTicketRequest,
        from: IpAddr,
        now: u64,
    ) -> Result<DoctorTicketGrant, Refusal> {
        let Some((relay, address)) = self
            .relays
            .get(&request.relay_id)
            .and_then(|r| Some((r, r.doctor?)))
        else {
            return Err(Refusal::new(
                404,
                "no_doctor",
                "that relay doesn't run bufferbloat tests",
            ));
        };
        let limits = &self.config.doctor;
        let network = self.doctor_network_tag(from);
        let mut counts = self.doctor.lock().expect("doctor lock");
        let (hour, day) = (now / 3600, now / 86_400);
        if counts.hour != hour {
            counts.hour = hour;
            counts.per_hour.clear();
            counts.per_relay.clear();
        }
        if counts.day != day {
            counts.day = day;
            counts.per_day.clear();
        }
        let this_hour = counts.per_hour.get(&network).copied().unwrap_or(0);
        let today = counts.per_day.get(&network).copied().unwrap_or(0);
        if today >= limits.per_network_per_day {
            return Err(Refusal::new(
                429,
                "doctor_limit",
                "too many bufferbloat tests from this network today",
            )
            .retry(86_400 - now % 86_400));
        }
        if this_hour >= limits.per_network_per_hour {
            return Err(Refusal::new(
                429,
                "doctor_limit",
                "too many bufferbloat tests from this network this hour",
            )
            .retry(3600 - now % 3600));
        }
        if counts.per_relay.get(&relay.id).copied().unwrap_or(0) >= limits.per_relay_per_hour {
            return Err(Refusal::new(
                503,
                "doctor_busy",
                "that relay has run its share of bufferbloat tests this hour",
            )
            .retry(3600 - now % 3600));
        }
        let random = keys::random_32().map_err(|_| internal())?;
        let ticket = Ticket {
            id: random[..ticket::ID_LEN].try_into().expect("16 bytes"),
            relay_id: relay.id,
            issued_at: now,
            expires_at: now + limits.ticket_lifetime_secs,
            max_bytes: limits.max_bytes,
        };
        *counts.per_hour.entry(network).or_default() += 1;
        *counts.per_day.entry(network).or_default() += 1;
        *counts.per_relay.entry(relay.id).or_default() += 1;
        Ok(DoctorTicketGrant {
            ticket: base64::engine::general_purpose::STANDARD
                .encode(self.issuer.sign_ticket(&ticket)),
            relay_id: relay.id,
            address: address.to_string(),
            expires_at: ticket.expires_at,
            max_bytes: ticket.max_bytes,
        })
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
        // A client built with another definition of this region would send
        // traffic the exit drops, silently: refuse it, and say why.
        if let Some(theirs) = &request.definitions {
            if game.routing_digest(&request.region).as_deref() != Some(theirs.as_str()) {
                return Err(Refusal::new(
                    409,
                    "definitions_differ",
                    format!(
                        "this copy of PingPath has a different definition of {}'s servers than the relays; update PingPath",
                        game.name
                    ),
                ));
            }
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
        let renew_until = now + limits.max_lifetime_secs;
        let renew_secret = self.renew_secret(&claims, renew_until);
        sessions.insert(
            session_id,
            Record {
                client_key,
                exit_id: exit.id,
                game: game.id.clone(),
                region: request.region.clone(),
                renew_until,
                expires_at: claims.expires_at,
                last_issued: now,
                network,
            },
        );
        Ok(self.grant(&claims, &token, renew_until, Some(&renew_secret), exit))
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
                    .filter(|r| self.usable_exit(r))
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
                .filter(|r| serves(r) && self.usable_exit(r))
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
    pub fn renew(
        &self,
        request: &RenewRequest,
        from: IpAddr,
        now: u64,
    ) -> Result<SessionGrant, Refusal> {
        let result = self.try_renew(request, from, now);
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

    fn try_renew(
        &self,
        request: &RenewRequest,
        from: IpAddr,
        now: u64,
    ) -> Result<SessionGrant, Refusal> {
        let session_id: SessionId = unhex(&request.session_id).ok_or_else(unknown_session)?;
        let secret = base64_decode(&request.renew_secret).ok_or_else(unknown_session)?;
        let mut sessions = self.sessions.lock().expect("sessions lock");
        if now >= sessions.last_prune + PRUNE_INTERVAL_SECS {
            sessions.prune(now);
        }
        if !sessions.records.contains_key(&session_id) {
            self.adopt(&mut sessions, request, session_id, &secret, from, now)?;
        }
        let record = sessions
            .records
            .get_mut(&session_id)
            .ok_or_else(unknown_session)?;
        if !self.owns(record, &session_id, &secret) {
            return Err(unknown_session());
        }
        let limits = &self.config.sessions;
        let renew_until = record.renew_until;
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
        let exit = self
            .relays
            .get(&record.exit_id)
            .ok_or_else(unknown_session)?;
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

    /// Takes back a session a restart forgot: its token is ours and still
    /// valid, and the secret is the one derived for it, so the client is
    /// the one it was issued to. It counts against its network again but
    /// is never refused for that: it is already in a match.
    fn adopt(
        &self,
        sessions: &mut Sessions,
        request: &RenewRequest,
        session_id: SessionId,
        secret: &[u8; 32],
        from: IpAddr,
        now: u64,
    ) -> Result<(), Refusal> {
        let token = base64::engine::general_purpose::STANDARD
            .decode(&request.token)
            .map_err(|_| unknown_session())?;
        let claims = Verifier::new(vec![self.issuer.public_key()])
            .verify(&token, now)
            .map_err(|_| unknown_session())?;
        let expected = self.renew_secret(&claims, request.renew_until);
        if claims.session_id != session_id
            || !constant_time_eq(secret, &expected)
            || sessions.by_client_key.contains_key(&claims.client_key)
            || sessions.closed.contains_key(&session_id)
        {
            return Err(unknown_session());
        }
        sessions.insert(
            session_id,
            Record {
                client_key: claims.client_key,
                exit_id: claims.exit_id,
                game: claims.game.clone(),
                region: claims.region.clone(),
                renew_until: request.renew_until,
                expires_at: claims.expires_at,
                last_issued: claims.issued_at,
                network: self.network_tag(from),
            },
        );
        tracing::info!(session = %request.session_id, "session adopted after a restart");
        Ok(())
    }

    /// Ends a session at the client's request, freeing its place in its
    /// network's and exit's limits. Unknown sessions are already gone.
    pub fn close(&self, request: &CloseRequest) -> Result<(), Refusal> {
        let session_id: SessionId = unhex(&request.session_id).ok_or_else(unknown_session)?;
        let secret = base64_decode(&request.renew_secret).ok_or_else(unknown_session)?;
        let mut sessions = self.sessions.lock().expect("sessions lock");
        let Some(record) = sessions.records.get(&session_id) else {
            return Ok(());
        };
        if !self.owns(record, &session_id, &secret) {
            self.metrics.refused("unknown_session");
            return Err(unknown_session());
        }
        if let Some(record) = sessions.remove(&session_id) {
            let until = record.expires_at.saturating_add(CLOCK_SKEW_SECS);
            sessions.closed.insert(session_id, until);
        }
        self.metrics.closed();
        tracing::info!(session = %request.session_id, "session closed by the client");
        Ok(())
    }

    /// Whether `secret` is the renewal secret of this session.
    fn owns(&self, record: &Record, session_id: &SessionId, secret: &[u8; 32]) -> bool {
        let claims = Claims {
            session_id: *session_id,
            client_key: record.client_key,
            exit_id: record.exit_id,
            issued_at: 0,
            expires_at: 0,
            game: record.game.clone(),
            region: record.region.clone(),
        };
        constant_time_eq(secret, &self.renew_secret(&claims, record.renew_until))
    }

    /// HMAC over what the session is: it can't be guessed from the token,
    /// which relays see, and needs no storage.
    fn renew_secret(&self, claims: &Claims, renew_until: u64) -> [u8; 32] {
        let mut message = Vec::with_capacity(128);
        message.extend_from_slice(&claims.session_id);
        message.extend_from_slice(&claims.client_key);
        message.extend_from_slice(&claims.exit_id.to_be_bytes());
        message.extend_from_slice(&renew_until.to_be_bytes());
        for text in [&claims.game, &claims.region] {
            message.extend_from_slice(&(text.len() as u32).to_be_bytes());
            message.extend_from_slice(text.as_bytes());
        }
        hmac::sign(&self.renew_key, &message)
            .as_ref()
            .try_into()
            .expect("HMAC-SHA256 is 32 bytes")
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

    /// Stores a relay's latency measurements, if `api_key` belongs to it,
    /// and returns the samples it ignored.
    pub fn report_latency(
        &self,
        api_key: &str,
        report: &LatencyReport,
        now: u64,
    ) -> Result<Vec<Ignored>, Refusal> {
        let result = self.try_report_latency(api_key, report, now);
        match &result {
            Ok(ignored) => {
                self.metrics.latency_report();
                for sample in ignored {
                    self.metrics.ignored(sample.reason);
                }
                if ignored.is_empty() {
                    tracing::debug!(
                        relay = report.relay_id,
                        samples = report.samples.len(),
                        "latency report"
                    );
                } else {
                    tracing::warn!(relay = report.relay_id, ignored = ?ignored, "latency report: some samples ignored");
                }
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
    ) -> Result<Vec<Ignored>, Refusal> {
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
        if report.samples.len() > MAX_SAMPLES_PER_REPORT
            || report.regions.len() > MAX_SAMPLES_PER_REPORT
        {
            return Err(Refusal::new(400, "bad_report", "too many samples"));
        }
        // Each sample is judged on its own: a relay running other game
        // definitions than ours (mid-deploy) loses only those rows.
        let mut ignored = Vec::new();
        if let Some(key) = &report.public_key {
            let registered = self
                .relays
                .get(&relay)
                .and_then(|r| r.public_key.as_deref());
            let mut mismatched = self.key_mismatch.lock().expect("key mismatch lock");
            if registered == Some(key.trim()) {
                if mismatched.remove(&relay) {
                    tracing::info!(relay, "the exit's key matches the registry again");
                }
            } else {
                if mismatched.insert(relay) {
                    tracing::error!(
                        relay,
                        "the exit's WireGuard key isn't the registry's: it is not offered as an exit until one of them is fixed"
                    );
                }
                ignored.push(Ignored {
                    peer_id: None,
                    game: None,
                    region: None,
                    reason: "exit_key_mismatch",
                });
            }
        }
        let mut latency = self.latency.lock().expect("latency lock");
        for sample in &report.samples {
            let reason = if !valid_measurement(sample.rtt_ms, sample.loss_pct)
                || !valid_counts(sample.sent, sample.received)
            {
                Some("invalid_measurement")
            } else if sample.peer_id == relay || !self.relays.contains_key(&sample.peer_id) {
                Some("unknown_peer")
            } else {
                None
            };
            match reason {
                Some(reason) => ignored.push(Ignored {
                    peer_id: Some(sample.peer_id),
                    game: None,
                    region: None,
                    reason,
                }),
                None => latency
                    .entry((relay, sample.peer_id))
                    .or_default()
                    .push(Round {
                        rtt_ms: sample.rtt_ms,
                        loss_pct: sample.loss_pct,
                        sent: sample.sent,
                        received: sample.received,
                        at: now,
                    }),
            }
        }
        drop(latency);
        // Only for a region this relay serves as an exit, of a game that may
        // be routed and has that region.
        let exit_serves = |region: &str| {
            self.relays
                .get(&relay)
                .is_some_and(|r| r.has_role(Role::Exit) && r.regions.iter().any(|r| r == region))
        };
        let mut regions = self.region_latency.lock().expect("region latency lock");
        for sample in &report.regions {
            let game = self.games.get(&sample.game);
            let reason = if !valid_measurement(sample.rtt_ms, sample.loss_pct)
                || !valid_counts(sample.sent, sample.received)
            {
                Some("invalid_measurement")
            } else if game.is_none_or(|g| g.status.routing_policy() == RoutingPolicy::Refuse) {
                Some("game_not_routable")
            } else if game.is_none_or(|g| g.region(&sample.region).is_none())
                || !exit_serves(&sample.region)
            {
                Some("region_not_served")
            } else {
                None
            };
            match reason {
                Some(reason) => ignored.push(Ignored {
                    peer_id: None,
                    game: Some(sample.game.clone()),
                    region: Some(sample.region.clone()),
                    reason,
                }),
                None => regions
                    .entry((relay, sample.game.clone(), sample.region.clone()))
                    .or_default()
                    .push(Round {
                        rtt_ms: sample.rtt_ms,
                        loss_pct: sample.loss_pct,
                        sent: sample.sent,
                        received: sample.received,
                        at: now,
                    }),
            }
        }
        Ok(ignored)
    }

    /// The latest relay-to-relay measurements, oldest dropped.
    pub fn latency(&self, now: u64) -> Vec<LatencyRow> {
        let mut latency = self.latency.lock().expect("latency lock");
        latency.retain(|_, rounds| rounds.keep_fresh(now));
        let mut rows: Vec<LatencyRow> = latency
            .iter()
            .filter_map(|(&(relay_id, peer_id), rounds)| {
                let (rtt_ms, loss_pct, samples, age_secs) = rounds.pooled(now)?;
                Some(LatencyRow {
                    relay_id,
                    peer_id,
                    rtt_ms,
                    loss_pct,
                    samples,
                    age_secs,
                })
            })
            .collect();
        rows.sort_by_key(|r| (r.relay_id, r.peer_id));
        rows
    }

    /// The latest exit-to-region measurements, oldest dropped.
    pub fn region_latency(&self, now: u64) -> Vec<RegionRow> {
        let mut regions = self.region_latency.lock().expect("region latency lock");
        regions.retain(|_, rounds| rounds.keep_fresh(now));
        let mut rows: Vec<RegionRow> = regions
            .iter()
            .filter_map(|((relay_id, game, region), rounds)| {
                let (rtt_ms, loss_pct, samples, age_secs) = rounds.pooled(now)?;
                Some(RegionRow {
                    relay_id: *relay_id,
                    game: game.clone(),
                    region: region.clone(),
                    rtt_ms,
                    loss_pct,
                    samples,
                    age_secs,
                })
            })
            .collect();
        rows.sort_by(|a, b| {
            (a.relay_id, &a.game, &a.region).cmp(&(b.relay_id, &b.game, &b.region))
        });
        rows
    }

    /// For doctor tickets: /24 for IPv4 and /48 for IPv6. A home's ISP
    /// hands it a /56 or a /48, so counting /64s would give one home
    /// hundreds of allowances. Tagged apart from session counts.
    fn doctor_network_tag(&self, ip: IpAddr) -> NetworkTag {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
            v4 => v4,
        };
        let prefix: Vec<u8> = match ip {
            IpAddr::V4(v4) => [&b"doctor4"[..], &v4.octets()[..3]].concat(),
            IpAddr::V6(v6) => [&b"doctor6"[..], &v6.octets()[..6]].concat(),
        };
        let tag = hmac::sign(&self.network_key, &prefix);
        NetworkTag(
            tag.as_ref()[..8]
                .try_into()
                .expect("HMAC-SHA256 is 32 bytes"),
        )
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

/// Probe counts that make sense: none at all (a relay that doesn't count),
/// or a plausible round with no more answers than probes.
fn valid_counts(sent: u32, received: u32) -> bool {
    received <= sent && sent <= 1000
}

/// A round trip under a minute (or none: all lost) and a loss percentage.
fn valid_measurement(rtt_ms: Option<f64>, loss_pct: f64) -> bool {
    rtt_ms.is_none_or(|ms| ms.is_finite() && (0.0..60_000.0).contains(&ms))
        && loss_pct.is_finite()
        && (0.0..=100.0).contains(&loss_pct)
}

fn unknown_session() -> Refusal {
    Refusal::new(404, "unknown_session", "no such session, or it has ended")
}

fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
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
            // The entry runs bufferbloat tests.
            doctor: (id == 3).then(|| "192.0.2.3:8443".parse().unwrap()),
        };
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            admin_listen: None,
            issuer_seed_file: "unused".into(),
            games_dir: None,
            client_ip_header: None,
            sessions: limits,
            doctor: Default::default(),
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
        let state = ControlState::new(config, games(), &SEED).unwrap();
        let public = state.issuer_public_key();
        (state, public)
    }

    const SEED: [u8; 32] = [9; 32];

    #[test]
    fn doctor_tickets_name_their_relay_and_verify() {
        let (state, public) = state(SessionLimits::default());
        let from: IpAddr = "203.0.113.7".parse().unwrap();
        let grant = state
            .issue_doctor_ticket(&DoctorTicketRequest { relay_id: 3 }, from, 1_000)
            .unwrap();
        assert_eq!(grant.address, "192.0.2.3:8443");
        assert_eq!((grant.expires_at, grant.max_bytes), (1_060, 250_000_000));
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&grant.ticket)
            .unwrap();
        let ticket = Verifier::new(vec![public])
            .verify_ticket(&bytes, 1_001)
            .unwrap();
        assert_eq!(ticket.relay_id, 3);
        assert_eq!(ticket.max_bytes, 250_000_000);
        // Not a session token.
        assert!(Verifier::new(vec![public]).verify(&bytes, 1_001).is_err());
        // Relays without a doctor listener, and unknown ones, get none.
        for relay_id in [1, 99] {
            let refusal = state
                .issue_doctor_ticket(&DoctorTicketRequest { relay_id }, from, 1_000)
                .unwrap_err();
            assert_eq!((refusal.status, refusal.code), (404, "no_doctor"));
        }
        let view = state.relays().into_iter().find(|r| r.id == 3).unwrap();
        assert_eq!(view.doctor.as_deref(), Some("192.0.2.3:8443"));
    }

    #[test]
    fn doctor_tickets_count_a_home_ipv6_network_once_and_cap_each_relay() {
        let (state, _) = state(SessionLimits::default());
        let ask = |ip: &str, now: u64| {
            state.issue_doctor_ticket(
                &DoctorTicketRequest { relay_id: 3 },
                ip.parse().unwrap(),
                now,
            )
        };
        let hour = 3600;
        // Four /64s of one home's /48 share its allowance.
        for i in 0..4 {
            ask(&format!("2001:db8:1:{i}::1"), 10 * hour).unwrap();
        }
        let refusal = ask("2001:db8:1:ff::1", 10 * hour).unwrap_err();
        assert_eq!(refusal.code, "doctor_limit");
        ask("2001:db8:2::1", 10 * hour).unwrap();

        // A relay serves at most per_relay_per_hour tests, whoever asks.
        let mut limits = state.config.clone();
        limits.doctor.per_relay_per_hour = 3;
        let state = ControlState::new(limits, games(), &SEED).unwrap();
        for i in 0..3 {
            state
                .issue_doctor_ticket(
                    &DoctorTicketRequest { relay_id: 3 },
                    format!("198.51.{i}.1").parse().unwrap(),
                    10 * hour,
                )
                .unwrap();
        }
        let busy = state
            .issue_doctor_ticket(
                &DoctorTicketRequest { relay_id: 3 },
                "203.0.113.1".parse().unwrap(),
                10 * hour + 60,
            )
            .unwrap_err();
        assert_eq!((busy.status, busy.code), (503, "doctor_busy"));
        assert_eq!(busy.retry_after, Some(hour - 60));
        state
            .issue_doctor_ticket(
                &DoctorTicketRequest { relay_id: 3 },
                "203.0.113.1".parse().unwrap(),
                11 * hour,
            )
            .unwrap();
    }

    #[test]
    fn doctor_tickets_are_limited_per_network_per_hour_and_day() {
        let (state, _) = state(SessionLimits::default());
        let ask = |ip: &str, now: u64| {
            state.issue_doctor_ticket(
                &DoctorTicketRequest { relay_id: 3 },
                ip.parse().unwrap(),
                now,
            )
        };
        let hour = 3600;
        for _ in 0..4 {
            ask("203.0.113.7", 10 * hour).unwrap();
        }
        // The same /24.
        let refusal = ask("203.0.113.99", 10 * hour + 600).unwrap_err();
        assert_eq!((refusal.status, refusal.code), (429, "doctor_limit"));
        assert_eq!(refusal.retry_after, Some(hour - 600));
        // Another network isn't counted with it.
        ask("198.51.100.1", 10 * hour).unwrap();
        // The next hour allows more, up to ten in the day.
        for _ in 0..4 {
            ask("203.0.113.7", 11 * hour).unwrap();
        }
        ask("203.0.113.7", 12 * hour).unwrap();
        ask("203.0.113.7", 12 * hour).unwrap();
        let refusal = ask("203.0.113.7", 13 * hour).unwrap_err();
        assert_eq!(refusal.code, "doctor_limit");
        assert_eq!(refusal.retry_after, Some(24 * hour - 13 * hour));
        ask("203.0.113.7", 24 * hour).unwrap();
    }

    /// The same control plane after a restart: same config and seed, no
    /// sessions.
    fn restart(state: &ControlState) -> ControlState {
        ControlState::new(state.config.clone(), games(), &SEED).unwrap()
    }

    fn renewal(grant: &SessionGrant) -> RenewRequest {
        RenewRequest {
            session_id: grant.session_id.clone(),
            renew_secret: grant.renew_secret.clone().unwrap(),
            token: grant.token.clone(),
            renew_until: grant.renew_until,
        }
    }

    fn request(game: &str, region: &str, exit_id: Option<u32>) -> SessionRequest {
        SessionRequest {
            client_public_key: pingpath_tunnel::keys::WireGuardKeys::generate()
                .unwrap()
                .public_base64(),
            game: game.into(),
            region: region.into(),
            exit_id,
            definitions: None,
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
        let renew = renewal(&grant);

        let refusal = state.renew(&renew, ip(HOME), NOW + 30).unwrap_err();
        assert_eq!(
            (refusal.code, refusal.retry_after),
            ("renewal_too_soon", Some(30))
        );

        let renewed = state.renew(&renew, ip(HOME), NOW + 600).unwrap();
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
        state.renew(&renew, ip(HOME), NOW + 1400).unwrap();
        state.renew(&renew, ip(HOME), NOW + 2200).unwrap();
        let last = state.renew(&renew, ip(HOME), NOW + 3000).unwrap();
        assert_eq!(last.expires_at, NOW + 3600);
        assert_eq!(
            state.renew(&renew, ip(HOME), NOW + 3550).unwrap_err().code,
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
            renew_secret: keys::encode(&[7; 32]),
            ..renewal(&grant)
        };
        assert_eq!(
            state.renew(&wrong, ip(HOME), NOW + 300).unwrap_err().code,
            "unknown_session"
        );
        let garbage = RenewRequest {
            session_id: "xyz".into(),
            renew_secret: "!".into(),
            token: "!".into(),
            renew_until: 0,
        };
        assert_eq!(
            state.renew(&garbage, ip(HOME), NOW + 300).unwrap_err().code,
            "unknown_session"
        );
        // Once its last token can't be used, a session is gone for good.
        assert_eq!(
            state
                .renew(&renewal(&grant), ip(HOME), NOW + 900 + CLOCK_SKEW_SECS)
                .unwrap_err()
                .code,
            "unknown_session"
        );
    }

    #[test]
    fn renewal_survives_a_restart() {
        // ADR 006: a restart must not push optimized players off the
        // relays at their next renewal.
        let (before, public) = state(SessionLimits::default());
        let grant = before
            .issue(
                &request("counter-strike-2", "eu-central", None),
                ip(HOME),
                NOW,
            )
            .unwrap();
        let other = before
            .issue(
                &request("counter-strike-2", "eu-central", None),
                ip(HOME),
                NOW,
            )
            .unwrap();
        let after = restart(&before);
        assert_eq!(after.active_sessions(), 0);

        // Someone else's secret, or a token from another session, proves
        // nothing.
        for forged in [
            RenewRequest {
                renew_secret: other.renew_secret.clone().unwrap(),
                ..renewal(&grant)
            },
            RenewRequest {
                token: other.token.clone(),
                ..renewal(&grant)
            },
            RenewRequest {
                renew_until: grant.renew_until + 3600,
                ..renewal(&grant)
            },
        ] {
            assert_eq!(
                after.renew(&forged, ip(HOME), NOW + 600).unwrap_err().code,
                "unknown_session"
            );
        }

        let renewed = after.renew(&renewal(&grant), ip(HOME), NOW + 600).unwrap();
        let claims = token(&renewed, public, NOW + 600);
        let first = token(&grant, public, NOW);
        assert_eq!(
            (claims.session_id, claims.client_key, claims.exit_id),
            (first.session_id, first.client_key, first.exit_id)
        );
        assert_eq!(renewed.renew_until, grant.renew_until);
        assert_eq!(after.active_sessions(), 1, "adopted and counted again");
        // Adopted once: the next renewal is an ordinary one.
        assert_eq!(
            after
                .renew(&renewal(&grant), ip(HOME), NOW + 630)
                .unwrap_err()
                .code,
            "renewal_too_soon"
        );
    }

    #[test]
    fn closing_frees_the_networks_place() {
        let (state, _) = state(SessionLimits {
            max_active_per_network: 1,
            ..Default::default()
        });
        let cs2 = || request("counter-strike-2", "eu-central", None);
        let grant = state.issue(&cs2(), ip(HOME), NOW).unwrap();
        assert_eq!(
            state.issue(&cs2(), ip(HOME), NOW).unwrap_err().code,
            "network_limit"
        );
        let close = |secret: String| CloseRequest {
            session_id: grant.session_id.clone(),
            renew_secret: secret,
        };
        assert_eq!(
            state
                .close(&close(keys::encode(&[7; 32])))
                .unwrap_err()
                .code,
            "unknown_session"
        );
        state
            .close(&close(grant.renew_secret.clone().unwrap()))
            .unwrap();
        assert_eq!(state.active_sessions(), 0);
        // Closing twice is fine: the client may not hear the first answer.
        state
            .close(&close(grant.renew_secret.clone().unwrap()))
            .unwrap();
        state.issue(&cs2(), ip(HOME), NOW).unwrap();
    }

    #[test]
    fn a_client_with_other_definitions_is_refused() {
        let (state, _) = state(SessionLimits::default());
        let game = games()
            .into_iter()
            .find(|g| g.id == "counter-strike-2")
            .unwrap();
        let ours = game.routing_digest("eu-central").unwrap();
        let with = |definitions: &str| SessionRequest {
            definitions: Some(definitions.into()),
            ..request("counter-strike-2", "eu-central", None)
        };
        assert_eq!(
            state
                .issue(&with("0000000000000000"), ip(HOME), NOW)
                .unwrap_err()
                .code,
            "definitions_differ"
        );
        state.issue(&with(&ours), ip(HOME), NOW).unwrap();
    }

    #[test]
    fn a_closed_session_cannot_be_renewed_back() {
        // Adoption is for sessions a restart forgot. One the client closed
        // must stay closed, or a network could issue, close and renew any
        // number of sessions while its count stays at zero.
        let (state, _) = state(SessionLimits::default());
        let grant = state
            .issue(
                &request("counter-strike-2", "eu-central", None),
                ip(HOME),
                NOW,
            )
            .unwrap();
        state
            .close(&CloseRequest {
                session_id: grant.session_id.clone(),
                renew_secret: grant.renew_secret.clone().unwrap(),
            })
            .unwrap();
        assert_eq!(
            state
                .renew(&renewal(&grant), ip(HOME), NOW + 600)
                .unwrap_err()
                .code,
            "unknown_session"
        );
        assert_eq!(state.active_sessions(), 0);
    }

    #[test]
    fn latency_reports_need_the_relays_own_key() {
        let (state, _) = state(SessionLimits::default());
        let sample = |peer_id, rtt_ms| LatencySample {
            peer_id,
            rtt_ms,
            loss_pct: 0.0,
            sent: 0,
            received: 0,
        };
        let report = LatencyReport {
            relay_id: 1,
            public_key: None,
            samples: vec![sample(2, Some(12.5)), sample(3, None)],
            regions: vec![],
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
            public_key: None,
            samples: vec![sample(1, Some(1.0))],
            regions: vec![],
        };
        let reasons = |report: &LatencyReport| {
            state
                .report_latency("key-1", report, NOW)
                .unwrap()
                .into_iter()
                .map(|i| i.reason)
                .collect::<Vec<_>>()
        };
        assert_eq!(reasons(&to_self), ["unknown_peer"]);
        let negative = LatencyReport {
            relay_id: 1,
            public_key: None,
            samples: vec![sample(2, Some(-1.0)), sample(9, Some(1.0))],
            regions: vec![],
        };
        assert_eq!(reasons(&negative), ["invalid_measurement", "unknown_peer"]);
        assert!(state.latency(NOW).is_empty());

        assert_eq!(state.report_latency("key-1\n", &report, NOW), Ok(vec![]));
        let rows = state.latency(NOW + 5);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            LatencyRow {
                relay_id: 1,
                peer_id: 2,
                rtt_ms: Some(12.5),
                loss_pct: 0.0,
                samples: 0,
                age_secs: 5
            }
        );
        assert!(state.latency(NOW + LATENCY_MAX_AGE_SECS + 1).is_empty());
    }

    #[test]
    fn exits_report_the_regions_they_serve() {
        let (state, _) = state(SessionLimits::default());
        let region = |region: &str, rtt_ms| RegionSample {
            game: "counter-strike-2".into(),
            region: region.into(),
            rtt_ms,
            loss_pct: 0.0,
            sent: 0,
            received: 0,
        };
        let report = |relay_id, regions| LatencyReport {
            relay_id,
            public_key: None,
            samples: vec![],
            regions,
        };
        // Relay 2 is an exit serving both regions; relay 1 only eu-central;
        // relay 3 is an entry.
        state
            .report_latency(
                "key-2",
                &report(
                    2,
                    vec![region("eu-central", Some(3.5)), region("eu-north", None)],
                ),
                NOW,
            )
            .unwrap();
        for (key, bad, reason) in [
            (
                "key-1",
                report(1, vec![region("eu-north", Some(1.0))]),
                "region_not_served",
            ),
            (
                "key-3",
                report(3, vec![region("eu-central", Some(1.0))]),
                "region_not_served",
            ),
            (
                "key-1",
                report(1, vec![region("eu-central", Some(f64::NAN))]),
                "invalid_measurement",
            ),
            // A game that is never routed: never probed, never accepted.
            (
                "key-1",
                report(
                    1,
                    vec![RegionSample {
                        game: "untested".into(),
                        ..region("eu-central", Some(1.0))
                    }],
                ),
                "game_not_routable",
            ),
        ] {
            let ignored = state.report_latency(key, &bad, NOW).unwrap();
            assert_eq!(ignored.len(), 1);
            assert_eq!(ignored[0].reason, reason);
        }
        let rows = state.region_latency(NOW + 1);
        assert_eq!(
            rows,
            [
                RegionRow {
                    relay_id: 2,
                    game: "counter-strike-2".into(),
                    region: "eu-central".into(),
                    rtt_ms: Some(3.5),
                    loss_pct: 0.0,
                    samples: 0,
                    age_secs: 1
                },
                RegionRow {
                    relay_id: 2,
                    game: "counter-strike-2".into(),
                    region: "eu-north".into(),
                    rtt_ms: None,
                    loss_pct: 0.0,
                    samples: 0,
                    age_secs: 1
                },
            ]
        );
        assert!(
            state
                .region_latency(NOW + LATENCY_MAX_AGE_SECS + 1)
                .is_empty()
        );
    }

    #[test]
    fn loss_is_judged_over_a_minute_of_probes() {
        // One lost probe in a round of five is 20% that round, but a
        // fraction of the minute's probes; a peer that stops answering
        // shows at once.
        let (state, _) = state(SessionLimits::default());
        let round = |rtt_ms: Option<f64>, received| LatencyReport {
            relay_id: 1,
            public_key: None,
            samples: vec![LatencySample {
                peer_id: 2,
                rtt_ms,
                loss_pct: (5 - received) as f64 * 20.0,
                sent: 5,
                received,
            }],
            regions: vec![],
        };
        for (i, (rtt, received)) in [(10.0, 5), (12.0, 4), (11.0, 5), (30.0, 5), (10.0, 5)]
            .into_iter()
            .enumerate()
        {
            let report = round(Some(rtt), received);
            assert_eq!(
                state.report_latency("key-1", &report, NOW + i as u64 * 10),
                Ok(vec![])
            );
        }
        let row = &state.latency(NOW + 40)[0];
        assert_eq!(
            (row.rtt_ms, row.loss_pct, row.samples),
            (Some(11.0), 4.0, 25),
            "median round trip, 1 lost of 25"
        );
        state
            .report_latency("key-1", &round(None, 0), NOW + 50)
            .unwrap();
        let row = &state.latency(NOW + 50)[0];
        assert_eq!((row.rtt_ms, row.samples), (None, 30));
        // Only the last six rounds count.
        for i in 0..6 {
            state
                .report_latency("key-1", &round(Some(10.0), 5), NOW + 60 + i)
                .unwrap();
        }
        let row = &state.latency(NOW + 70)[0];
        assert_eq!((row.loss_pct, row.samples), (0.0, 30));
        // Counts that can't be right are ignored.
        let mut bad = round(Some(10.0), 5);
        bad.samples[0].received = 6;
        assert_eq!(
            state.report_latency("key-1", &bad, NOW + 80).unwrap()[0].reason,
            "invalid_measurement"
        );
    }

    #[test]
    fn an_exit_with_the_wrong_key_is_not_offered_as_an_exit() {
        let (state, _) = state(SessionLimits::default());
        let registered = state.relays()[1].public_key.clone().unwrap();
        let report = |key: &str| LatencyReport {
            relay_id: 2,
            public_key: Some(key.into()),
            samples: vec![],
            regions: vec![],
        };
        let wrong = pingpath_tunnel::keys::WireGuardKeys::generate()
            .unwrap()
            .public_base64();
        let ignored = state.report_latency("key-2", &report(&wrong), NOW).unwrap();
        assert_eq!(ignored[0].reason, "exit_key_mismatch");
        // Still listed (it is an entry too), but without a key to use.
        assert_eq!(state.relays()[1].public_key, None);
        assert_eq!(
            state
                .issue(
                    &request("counter-strike-2", "eu-north", Some(2)),
                    ip(HOME),
                    NOW
                )
                .unwrap_err()
                .code,
            "unknown_exit"
        );
        assert_eq!(
            state.report_latency("key-2", &report(&registered), NOW),
            Ok(vec![])
        );
        assert_eq!(state.relays()[1].public_key, Some(registered));
        state
            .issue(
                &request("counter-strike-2", "eu-north", Some(2)),
                ip(HOME),
                NOW,
            )
            .unwrap();
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
