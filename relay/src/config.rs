//! Relay configuration, read from a TOML file (see `relay/relay.example.toml`).

use std::collections::HashSet;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Forwards clients' encrypted frames to exits; holds no tunnel keys.
    Entry,
    /// Terminates tunnels and forwards to allowlisted game servers.
    Exit,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Registry ID; must match the control plane's and the `exit_id` in tokens.
    pub id: u32,
    pub name: String,
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Health and metrics; keep it private (firewalled or localhost).
    #[serde(default = "default_http")]
    pub http: Option<SocketAddr>,
    pub roles: Vec<Role>,
    /// X25519 private key (base64), required for the exit role.
    #[serde(default)]
    pub private_key_file: Option<PathBuf>,
    /// Exit role: the game regions this relay serves, e.g. `["eu-central"]`.
    /// Region IDs are shared across game definitions (PLAN.md rule 3: an
    /// exit only forwards to servers in its own region).
    #[serde(default)]
    pub regions: Vec<String>,
    /// Ed25519 public keys of the control plane (base64). Several during a rotation.
    pub issuer_public_keys: Vec<String>,
    /// Game definitions; the ones built into the binary when unset.
    #[serde(default)]
    pub games_dir: Option<PathBuf>,
    #[serde(default)]
    pub limits: Limits,
    /// The other relays: exits this relay may forward to, and peers to probe.
    #[serde(default)]
    pub relays: Vec<Peer>,
    #[serde(default)]
    pub probe: ProbeConfig,
    #[serde(default)]
    pub control: Option<Control>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub id: u32,
    pub name: String,
    pub address: SocketAddr,
    pub roles: Vec<Role>,
}

/// Every limit a relay enforces (docs/decisions/004-relay-wire-protocol.md).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    /// Sessions per relay, each role counted separately.
    pub max_sessions: usize,
    /// Per session, each way.
    pub packets_per_second: u64,
    /// Per session, each way.
    pub bits_per_second: u64,
    /// UDP flows (client port to server address) per session.
    pub max_flows_per_session: usize,
    /// New flows a session may open per second (bursts up to the flow limit).
    pub new_flows_per_second: u64,
    /// UDP flows per relay. Keep below the ephemeral port range.
    pub max_flows: i64,
    pub flow_idle_secs: u64,
    /// A flow whose server never answers is closed after this long.
    pub unanswered_flow_secs: u64,
    /// HELLO frames per source network (/24, or /64 for IPv6) per second.
    pub handshakes_per_second: u64,
    /// PROBE frames per source network per second. Separate from HELLO, so
    /// players measuring behind one carrier NAT don't use up each other's
    /// handshakes; a reply is never larger than its PROBE.
    pub probes_per_second: u64,
    /// HELLO frames per second, from everyone.
    pub hellos_per_second: u64,
    /// To one game server address and port, across all sessions.
    pub destination_packets_per_second: u64,
    pub destination_bits_per_second: u64,
    /// An exit session without a WireGuard handshake is dropped after this long.
    pub pending_session_secs: u64,
    /// An entry session that hears nothing from its exit is dropped after this long.
    pub entry_idle_secs: u64,
    /// The exit stops sending on a return path silent for this long.
    pub stale_path_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_sessions: 10_000,
            packets_per_second: 1_000,
            bits_per_second: 2_000_000,
            max_flows_per_session: 8,
            new_flows_per_second: 2,
            max_flows: 20_000,
            flow_idle_secs: 120,
            unanswered_flow_secs: 10,
            handshakes_per_second: 20,
            probes_per_second: 50,
            hellos_per_second: 2_000,
            destination_packets_per_second: 20_000,
            destination_bits_per_second: 100_000_000,
            pending_session_secs: 10,
            entry_idle_secs: 15,
            stale_path_secs: 5,
        }
    }
}

impl Limits {
    pub fn flow_idle(&self) -> Duration {
        Duration::from_secs(self.flow_idle_secs)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProbeConfig {
    /// Seconds between rounds of probes to every peer; 0 turns probing off.
    pub interval_secs: u64,
    pub probes_per_round: u32,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            interval_secs: 10,
            probes_per_round: 5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    /// Base URL, e.g. `https://control.example.net`.
    pub url: String,
    /// File holding this relay's API key for reporting to the control plane.
    pub api_key_file: PathBuf,
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 51820))
}

fn default_http() -> Option<SocketAddr> {
    Some(SocketAddr::from(([127, 0, 0, 1], 9100)))
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

    pub fn has_role(&self, role: Role) -> bool {
        self.roles.contains(&role)
    }

    /// Catches mistakes that would otherwise surface as silent drops.
    pub fn validate(&self) -> Result<(), String> {
        let mut problems = Vec::new();
        if self.roles.is_empty() {
            problems.push("roles must include \"entry\", \"exit\" or both".to_string());
        }
        if self.has_role(Role::Exit) && self.private_key_file.is_none() {
            problems.push("the exit role needs private_key_file".to_string());
        }
        if self.has_role(Role::Exit) && self.regions.is_empty() {
            problems.push("the exit role needs regions, the game regions it serves".to_string());
        }
        if self.issuer_public_keys.is_empty() {
            problems
                .push("issuer_public_keys must list at least one control plane key".to_string());
        }
        for key in &self.issuer_public_keys {
            if pingpath_tunnel::keys::decode(key).is_err() {
                problems.push(format!("issuer key \"{key}\" is not a base64 32-byte key"));
            }
        }
        let mut ids = HashSet::from([self.id]);
        for peer in &self.relays {
            if !ids.insert(peer.id) {
                problems.push(format!(
                    "relay id {} appears twice (or is this relay's own id)",
                    peer.id
                ));
            }
        }
        if self.has_role(Role::Entry) && !self.relays.iter().any(|p| p.roles.contains(&Role::Exit))
        {
            problems.push(
                "the entry role needs at least one relay with the exit role in [[relays]]"
                    .to_string(),
            );
        }
        let l = &self.limits;
        if l.max_sessions == 0
            || l.packets_per_second == 0
            || l.bits_per_second == 0
            || l.max_flows_per_session == 0
        {
            problems.push("limits must be greater than zero".to_string());
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

    const KEY: &str = "hSDwCYkwp1R0i33ctD73Wg2/Og0mOBr066SpjqqbTmo=";

    fn parse(text: &str) -> Result<Config, String> {
        let config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        config.validate().map(|()| config)
    }

    #[test]
    fn example_config_is_valid() {
        let text = include_str!("../relay.example.toml");
        let config = parse(text).unwrap();
        assert!(config.has_role(Role::Entry) && config.has_role(Role::Exit));
        assert_eq!(config.limits.max_flows_per_session, 8);
        assert_eq!(config.regions, ["eu-central"]);
    }

    #[test]
    fn minimal_exit_gets_defaults() {
        let config = parse(&format!(
            "id = 1\nname = \"fra-1\"\nroles = [\"exit\"]\nregions = [\"eu-central\"]\nprivate_key_file = \"k\"\nissuer_public_keys = [\"{KEY}\"]\n"
        ))
        .unwrap();
        assert_eq!(config.listen, default_listen());
        assert_eq!(config.http, default_http());
        assert_eq!(config.limits.packets_per_second, 1_000);
        assert_eq!(config.probe.interval_secs, 10);
    }

    #[test]
    fn reports_every_mistake() {
        let err = parse("id = 1\nname = \"x\"\nroles = [\"exit\", \"entry\"]\nissuer_public_keys = [\"nope\"]\n").unwrap_err();
        assert!(err.contains("private_key_file"), "{err}");
        assert!(err.contains("needs regions"), "{err}");
        assert!(err.contains("not a base64"), "{err}");
        assert!(
            err.contains("at least one relay with the exit role"),
            "{err}"
        );
        assert!(parse("id = 1\nname = \"x\"\nroles = [\"exit\"]\nbogus = 1\n").is_err());
    }
}
