//! The control plane's API from the client's side (docs/decisions/006-control-plane.md).
//! Blocking (ureq); the engine calls it from a blocking thread. Nothing
//! the control plane says can widen what is routed: routes come only from
//! the local game definitions (docs/decisions/008-split-tunnel-routing.md).

use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;
use serde_json::json;

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Relay {
    pub id: u32,
    pub name: String,
    pub address: SocketAddr,
    pub roles: Vec<String>,
    #[serde(default)]
    pub regions: Vec<String>,
    /// Exits: their WireGuard public key (base64).
    #[serde(default)]
    pub public_key: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
}

impl Relay {
    pub fn is_entry(&self) -> bool {
        self.roles.iter().any(|r| r == "entry")
    }

    pub fn is_exit(&self) -> bool {
        self.roles.iter().any(|r| r == "exit")
    }

    pub fn serves(&self, region: &str) -> bool {
        self.is_exit() && self.regions.iter().any(|r| r == region)
    }
}

/// A granted or renewed session.
#[derive(Clone, PartialEq, Deserialize)]
pub struct Grant {
    pub session_id: String,
    /// The token, base64 as the API sends it; see [`Grant::token_bytes`].
    pub token: String,
    pub issued_at: u64,
    pub expires_at: u64,
    pub renew_until: u64,
    #[serde(default)]
    pub renew_secret: Option<String>,
    pub game: String,
    pub region: String,
    pub exit: Relay,
    #[serde(default)]
    pub entries: Vec<Relay>,
}

impl fmt::Debug for Grant {
    // The token and the renewal secret never reach a log line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Grant")
            .field("session_id", &self.session_id)
            .field("expires_at", &self.expires_at)
            .field("renew_until", &self.renew_until)
            .field("game", &self.game)
            .field("region", &self.region)
            .field("exit", &self.exit.name)
            .field("entries", &self.entries.len())
            .finish_non_exhaustive()
    }
}

impl Grant {
    pub fn token_bytes(&self) -> Result<Vec<u8>, ControlError> {
        base64::engine::general_purpose::STANDARD
            .decode(&self.token)
            .map_err(|_| ControlError::Invalid("the token is not base64".into()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct RelayLatency {
    pub relay_id: u32,
    pub peer_id: u32,
    pub rtt_ms: Option<f64>,
    pub loss_pct: f64,
    pub age_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RegionLatency {
    pub relay_id: u32,
    pub game: String,
    pub region: String,
    pub rtt_ms: Option<f64>,
    pub loss_pct: f64,
    pub age_secs: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Latency {
    pub latency: Vec<RelayLatency>,
    #[serde(default)]
    pub regions: Vec<RegionLatency>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    /// The control plane refused, with its stable code (ADR 006).
    #[error("the control plane refused ({code}): {message}")]
    Refused {
        status: u16,
        code: String,
        message: String,
        retry_after: Option<u64>,
    },
    #[error("cannot reach the control plane: {0}")]
    Network(String),
    #[error("unexpected answer from the control plane: {0}")]
    Invalid(String),
    #[error("no control plane is configured")]
    NotConfigured,
}

#[derive(Clone)]
pub struct ControlClient {
    base: String,
    agent: ureq::Agent,
}

impl ControlClient {
    pub fn new(base_url: &str) -> Result<Self, ControlError> {
        if base_url.trim().is_empty() {
            return Err(ControlError::NotConfigured);
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        Ok(Self {
            base: base_url.trim().trim_end_matches('/').to_string(),
            agent,
        })
    }

    pub fn relays(&self) -> Result<Vec<Relay>, ControlError> {
        #[derive(Deserialize)]
        struct Relays {
            relays: Vec<Relay>,
        }
        Ok(self.get::<Relays>("/v1/relays")?.relays)
    }

    pub fn latency(&self) -> Result<Latency, ControlError> {
        self.get("/v1/latency")
    }

    /// A session for `game` in `region` on `exit_id`, for the WireGuard key
    /// `client_key` (base64), made for this session alone.
    pub fn create_session(
        &self,
        client_key: &str,
        game: &str,
        region: &str,
        exit_id: Option<u32>,
    ) -> Result<Grant, ControlError> {
        let grant: Grant = self.post(
            "/v1/sessions",
            json!({ "client_public_key": client_key, "game": game, "region": region, "exit_id": exit_id }),
        )?;
        if grant.game != game
            || grant.region != region
            || exit_id.is_some_and(|id| id != grant.exit.id)
        {
            return Err(ControlError::Invalid(
                "the session is not for the game, region or exit asked for".into(),
            ));
        }
        if grant.exit.public_key.is_none() || grant.renew_secret.is_none() {
            return Err(ControlError::Invalid(
                "the session lacks the exit's key or the renewal secret".into(),
            ));
        }
        Ok(grant)
    }

    /// A fresh token for the same session. The current token and
    /// `renew_until` let a control plane that restarted take the session
    /// back.
    pub fn renew(&self, grant: &Grant, renew_secret: &str) -> Result<Grant, ControlError> {
        let session_id = grant.session_id.as_str();
        let grant: Grant = self.post(
            "/v1/sessions/renew",
            json!({
                "session_id": session_id,
                "renew_secret": renew_secret,
                "token": grant.token,
                "renew_until": grant.renew_until,
            }),
        )?;
        if grant.session_id != session_id {
            return Err(ControlError::Invalid(
                "renewal returned another session".into(),
            ));
        }
        Ok(grant)
    }

    /// Tells the control plane the session is over, so it stops counting
    /// against the player's network. A session it doesn't know is over too.
    pub fn close(&self, session_id: &str, renew_secret: &str) -> Result<(), ControlError> {
        let response = self
            .agent
            .post(&format!("{}/v1/sessions/close", self.base))
            .header("Content-Type", "application/json")
            .send(json!({ "session_id": session_id, "renew_secret": renew_secret }).to_string())
            .map_err(|e| ControlError::Network(e.to_string()))?;
        match response.status().as_u16() {
            200..=299 | 404 => Ok(()),
            _ => decode::<serde_json::Value>(response).map(drop),
        }
    }

    fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, ControlError> {
        let response = self
            .agent
            .get(&format!("{}{path}", self.base))
            .call()
            .map_err(|e| ControlError::Network(e.to_string()))?;
        decode(response)
    }

    fn post<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<T, ControlError> {
        let response = self
            .agent
            .post(&format!("{}{path}", self.base))
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(|e| ControlError::Network(e.to_string()))?;
        decode(response)
    }
}

fn decode<T: for<'de> Deserialize<'de>>(
    mut response: ureq::http::Response<ureq::Body>,
) -> Result<T, ControlError> {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| ControlError::Network(e.to_string()))?;
    if (200..300).contains(&status) {
        return serde_json::from_str(&text).map_err(|e| ControlError::Invalid(e.to_string()));
    }
    #[derive(Deserialize)]
    struct ApiError {
        error: String,
        message: String,
    }
    match serde_json::from_str::<ApiError>(&text) {
        Ok(e) => Err(ControlError::Refused {
            status,
            code: e.error,
            message: e.message,
            retry_after,
        }),
        Err(_) => Err(ControlError::Invalid(format!("HTTP {status}"))),
    }
}
