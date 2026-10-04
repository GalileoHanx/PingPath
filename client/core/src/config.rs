//! The service's settings, kept in `%ProgramData%\PingPath\client.toml`
//! and changed from the UI over IPC (docs/decisions/007-client-architecture.md).

use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ClientConfig {
    /// The control plane's base URL (HTTPS), e.g. `https://control.example.net`.
    /// Empty until the player or a tester sets it.
    pub control_url: String,
    /// Extra game definitions (e.g. a tester's echo target), next to the
    /// ones built in. Only readable by administrators, since the service
    /// runs as SYSTEM (docs/decisions/007).
    pub games_dir: Option<PathBuf>,
    /// The PingPath adapter's own address; the source of game packets
    /// while optimized.
    pub adapter_address: Ipv4Addr,
    /// Optimize only if the optimized ping beats the direct one by at least
    /// this many milliseconds ...
    pub min_improvement_ms: f64,
    /// ... or by at least this share of the direct ping (PLAN.md Phase 3).
    pub min_improvement_pct: f64,
    /// More detail in the service's log ("Verbose logging" in Settings).
    pub verbose_logging: bool,
    #[serde(with = "millis")]
    pub echo_interval: Duration,
    /// A path with no echo reply for this long has failed (rule 6).
    #[serde(with = "millis")]
    pub failure_after: Duration,
    /// Time allowed for moving to a standby path before falling back to
    /// the direct route.
    #[serde(with = "millis")]
    pub failover_budget: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            control_url: String::new(),
            games_dir: None,
            adapter_address: Ipv4Addr::new(10, 200, 0, 2),
            min_improvement_ms: 10.0,
            min_improvement_pct: 10.0,
            verbose_logging: false,
            echo_interval: Duration::from_millis(250),
            failure_after: Duration::from_secs(1),
            failover_budget: Duration::from_millis(700),
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
        if !self.control_url.is_empty() && !self.control_url.starts_with("https://") {
            // Plain HTTP is allowed only for a control plane on this machine
            // (development and tests); everything else must use TLS, because
            // the renewal secret travels in the request.
            let local = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
                .iter()
                .any(|prefix| self.control_url.starts_with(prefix));
            if !local {
                problems.push("control_url must start with https://".to_string());
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
