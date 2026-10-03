//! Valve's Steam Datagram Relay configuration: which relays serve which
//! point of presence (pop), as Valve publishes it for Steam clients.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::time::Duration;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct SdrConfig {
    pub revision: Option<u64>,
    pub pops: BTreeMap<String, Pop>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pop {
    pub description: Option<String>,
    pub relays: Vec<Relay>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Relay {
    pub address: Ipv4Addr,
    pub ports: Option<(u16, u16)>,
}

pub fn url(app_id: u32) -> String {
    format!("https://api.steampowered.com/ISteamApps/GetSDRConfig/v1/?appid={app_id}")
}

/// Downloads the relay list for a Steam app.
pub fn fetch(app_id: u32) -> Result<SdrConfig, String> {
    let url = url(app_id);
    tracing::info!(%url, "fetching Valve relay list");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .into();
    let body = agent
        .get(&url)
        .call()
        .map_err(|e| format!("cannot download {url}: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("cannot read {url}: {e}"))?;
    let config = parse(&body)?;
    tracing::info!(revision = ?config.revision, pops = config.pops.len(), "Valve relay list loaded");
    Ok(config)
}

/// Parses the relay list, tolerating missing or extra fields. If the shape
/// isn't recognised, the error describes what was found, so a report can
/// show how the format changed.
pub fn parse(json: &str) -> Result<SdrConfig, String> {
    let root: Value =
        serde_json::from_str(json).map_err(|e| format!("relay list is not JSON: {e}"))?;
    let keys = |v: &Value| match v.as_object() {
        Some(o) => o.keys().cloned().collect::<Vec<_>>().join(", "),
        None => "(not an object)".to_string(),
    };
    let Some(pops) = root.get("pops").and_then(Value::as_object) else {
        return Err(format!(
            "relay list has no \"pops\" object; top-level keys: {}",
            keys(&root)
        ));
    };

    let mut parsed = BTreeMap::new();
    for (code, pop) in pops {
        let relays: Vec<Relay> = pop
            .get("relays")
            .and_then(Value::as_array)
            .map(|relays| {
                relays
                    .iter()
                    .filter_map(|r| {
                        let address = r.get("ipv4")?.as_str()?.parse().ok()?;
                        let ports = r.get("port_range").and_then(Value::as_array).and_then(|p| {
                            let first = u16::try_from(p.first()?.as_u64()?).ok()?;
                            let last = u16::try_from(p.get(1)?.as_u64()?).ok()?;
                            Some((first, last))
                        });
                        Some(Relay { address, ports })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let description = pop.get("desc").and_then(Value::as_str).map(str::to_string);
        parsed.insert(
            code.clone(),
            Pop {
                description,
                relays,
            },
        );
    }
    if parsed.values().all(|p| p.relays.is_empty()) {
        let sample = pops.values().next().map(keys).unwrap_or_default();
        return Err(format!(
            "relay list has {} pops but no relays; a pop has keys: {sample}",
            pops.len()
        ));
    }
    Ok(SdrConfig {
        revision: root.get("revision").and_then(Value::as_u64),
        pops: parsed,
    })
}

/// Collapses relay addresses into sorted, de-duplicated /24 prefixes.
pub fn prefixes(relays: &[Relay]) -> Vec<String> {
    let mut nets: Vec<[u8; 3]> = relays
        .iter()
        .map(|r| {
            let [a, b, c, _] = r.address.octets();
            [a, b, c]
        })
        .collect();
    nets.sort_unstable();
    nets.dedup();
    nets.into_iter()
        .map(|[a, b, c]| format!("{a}.{b}.{c}.0/24"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "revision": 1700,
        "pops": {
            "fra": {
                "desc": "Frankfurt (Germany)",
                "geo": [8.68, 50.11],
                "relays": [
                    {"ipv4": "198.51.100.10", "port_range": [27015, 27060]},
                    {"ipv4": "198.51.100.11", "port_range": [27015, 27060]},
                    {"ipv4": "203.0.113.4", "port_range": [27015, 27060]}
                ]
            },
            "xyz": {"desc": "No relays here"}
        },
        "success": true
    }"#;

    #[test]
    fn parses_pops_and_relays() {
        let config = parse(SAMPLE).unwrap();
        assert_eq!(config.revision, Some(1700));
        let fra = &config.pops["fra"];
        assert_eq!(fra.description.as_deref(), Some("Frankfurt (Germany)"));
        assert_eq!(fra.relays.len(), 3);
        assert_eq!(fra.relays[0].ports, Some((27015, 27060)));
        assert!(config.pops["xyz"].relays.is_empty());
        assert_eq!(prefixes(&fra.relays), ["198.51.100.0/24", "203.0.113.0/24"]);
    }

    #[test]
    fn explains_an_unexpected_format() {
        let err = parse(r#"{"response": {"pops": []}}"#).unwrap_err();
        assert!(err.contains("top-level keys: response"), "{err}");
        let err = parse(r#"{"pops": {"fra": {"servers": []}}}"#).unwrap_err();
        assert!(err.contains("a pop has keys: servers"), "{err}");
    }
}
