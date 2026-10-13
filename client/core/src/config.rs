//! The service's settings, kept in the administrator-only
//! `%ProgramData%\PingPath\client.toml`. Players may change only verbose
//! logging and the improvement thresholds, over IPC; the rest is the
//! administrator's (docs/decisions/007-client-architecture.md).

use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ClientConfig {
    /// The control plane's base URL (HTTPS), e.g. `https://control.example.net`.
    /// Empty until an administrator sets it in the settings file; players
    /// can't change it over IPC (docs/decisions/007-client-architecture.md).
    pub control_url: String,
    /// Extra game definitions (e.g. a tester's echo target), next to the
    /// ones built in. Only readable by administrators, since the service
    /// runs as SYSTEM (docs/decisions/007).
    pub games_dir: Option<PathBuf>,
    /// The PingPath adapter's own address; the source of game packets
    /// while optimized. The default is the IPv4 dummy address (RFC 7600),
    /// which can't be a LAN host or a game server.
    pub adapter_address: Ipv4Addr,
    /// Optimize only if the optimized ping beats the direct one by at least
    /// this many milliseconds ...
    pub min_improvement_ms: f64,
    /// ... or by at least this share of the direct ping (PLAN.md Phase 3).
    pub min_improvement_pct: f64,
    /// More detail in the service's log ("Verbose logging" in Settings).
    pub verbose_logging: bool,
    /// Sending each game packet over two entries (ADR 010): never, always,
    /// or when the path shows loss or jitter.
    pub stability_mode: StabilityMode,
    #[serde(with = "millis")]
    pub echo_interval: Duration,
    /// A path with no authenticated traffic from the exit (echo replies,
    /// game packets, keepalives) for this long has failed (rule 6).
    #[serde(with = "millis")]
    pub failure_after: Duration,
    /// Time allowed for moving to a standby path before falling back to
    /// the direct route.
    #[serde(with = "millis")]
    pub failover_budget: Duration,
    /// Accept relays on private or loopback addresses (development and the
    /// network-namespace tests only). Off, relay addresses from the control
    /// plane must be public unicast before anything is routed or sent to
    /// them. Set only in the file, which only administrators can change.
    pub allow_local_relays: bool,
}

/// When to duplicate game traffic over a second entry (ADR 010).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StabilityMode {
    Off,
    /// While the path shows loss over 0.5% or jitter over 10 ms.
    Auto,
    On,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            control_url: String::new(),
            games_dir: None,
            adapter_address: Ipv4Addr::new(192, 0, 0, 8),
            min_improvement_ms: 10.0,
            min_improvement_pct: 10.0,
            verbose_logging: false,
            stability_mode: StabilityMode::Auto,
            echo_interval: Duration::from_millis(250),
            failure_after: Duration::from_secs(1),
            failover_budget: Duration::from_millis(700),
            allow_local_relays: false,
        }
    }
}

impl ClientConfig {
    /// Reads the settings, or the defaults if the file doesn't exist yet.
    pub fn load(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let config: Self =
                    toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
                config.validate()?;
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        // Write then rename, so a crash never leaves a half-written file.
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
    }

    /// Rejects settings that would break the watchdog's guarantees or point
    /// the service somewhere it shouldn't go.
    pub fn validate(&self) -> Result<(), String> {
        let mut problems = Vec::new();
        if !self.control_url.is_empty() {
            if let Err(problem) = check_control_url(&self.control_url) {
                problems.push(problem);
            }
        }
        if !(0.0..=1000.0).contains(&self.min_improvement_ms)
            || !(0.0..=100.0).contains(&self.min_improvement_pct)
        {
            problems.push("improvement thresholds are out of range".to_string());
        }
        let ms = |d: Duration| d.as_millis();
        if !(50..=1000).contains(&ms(self.echo_interval))
            || !(200..=5000).contains(&ms(self.failure_after))
            || ms(self.failure_after) <= ms(self.echo_interval)
            || !(100..=5000).contains(&ms(self.failover_budget))
        {
            problems.push("watchdog timings are out of range".to_string());
        }
        if self.adapter_address.is_unspecified() || self.adapter_address.is_multicast() {
            problems.push("adapter_address must be a unicast address".to_string());
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }
}

/// The control plane must use HTTPS; plain HTTP only to this machine
/// (development and tests), decided on the parsed host, not a prefix, since
/// the renewal secret travels in the request.
fn check_control_url(url: &str) -> Result<(), String> {
    let uri: ureq::http::Uri = url
        .parse()
        .map_err(|_| "control_url is not a valid URL".to_string())?;
    let authority = uri
        .authority()
        .ok_or_else(|| "control_url has no host".to_string())?;
    if authority.as_str().contains('@') {
        return Err("control_url must not contain a user name or password".to_string());
    }
    match uri.scheme_str() {
        Some("https") => Ok(()),
        Some("http") if matches!(authority.host(), "127.0.0.1" | "localhost" | "[::1]") => Ok(()),
        _ => Err("control_url must start with https://".to_string()),
    }
}

mod millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_millis() as u64)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_millis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_round_trip() {
        let config = ClientConfig::default();
        config.validate().unwrap();
        let text = toml::to_string_pretty(&config).unwrap();
        assert_eq!(toml::from_str::<ClientConfig>(&text).unwrap(), config);
    }

    #[test]
    fn refuses_unsafe_settings() {
        let bad = |change: fn(&mut ClientConfig)| {
            let mut config = ClientConfig::default();
            change(&mut config);
            config.validate().unwrap_err()
        };
        assert!(bad(|c| c.control_url = "http://control.example.net".into()).contains("https"));
        // Prefix tricks that point plain HTTP somewhere else.
        for url in [
            "http://127.0.0.1.evil.example/",
            "http://localhostevil.example/",
            "http://127.0.0.1@evil.example/",
        ] {
            let config = ClientConfig {
                control_url: url.into(),
                ..Default::default()
            };
            assert!(config.validate().is_err(), "{url}");
        }
        for url in [
            "https://control.example.net",
            "http://localhost:8080",
            "http://[::1]:8080/",
        ] {
            let config = ClientConfig {
                control_url: url.into(),
                ..Default::default()
            };
            config.validate().unwrap_or_else(|e| panic!("{url}: {e}"));
        }
        assert!(bad(|c| c.failure_after = Duration::from_millis(100)).contains("watchdog"));
        assert!(bad(|c| c.min_improvement_pct = 150.0).contains("thresholds"));
        let local = ClientConfig {
            control_url: "http://127.0.0.1:8080".into(),
            ..Default::default()
        };
        local.validate().unwrap();
    }

    #[test]
    fn missing_file_means_defaults() {
        let dir = std::env::temp_dir().join("pingpath-client-config-test-missing");
        assert_eq!(
            ClientConfig::load(&dir.join("client.toml")).unwrap(),
            ClientConfig::default()
        );
    }
}
