//! Who operates an address: the player's public IP and ISP, and the networks
//! along a route. Uses DNS-based services (OpenDNS and Google for "what is
//! my IP", Team Cymru for IP → ASN), so no HTTP API or account is needed.
//! See docs/decisions/003-probe-data-sources.md.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;

use crate::dns::{self, QueryType, Record};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetworkInfo {
    pub asn: u32,
    pub as_name: Option<String>,
    /// Country the network is registered in (not necessarily where the
    /// router is).
    pub country: Option<String>,
    pub prefix: Option<String>,
}

/// The public IPv4 address this machine's traffic leaves from.
pub fn public_ip() -> Result<Ipv4Addr, String> {
    let timeout = Duration::from_secs(2);
    let opendns = SocketAddr::from((Ipv4Addr::new(208, 67, 222, 222), 53));
    if let Ok(records) = dns::query(opendns, "myip.opendns.com", QueryType::A, timeout) {
        if let Some(Record::A(ip)) = records.first() {
            return Ok(*ip);
        }
    }
    // Google's authoritative server answers with the address that asked.
    let google = SocketAddr::from((Ipv4Addr::new(216, 239, 32, 10), 53));
    let records = dns::query(google, "o-o.myaddr.l.google.com", QueryType::Txt, timeout)
        .map_err(|e| format!("public IP lookup failed: {e}"))?;
    records
        .iter()
        .find_map(|r| match r {
            Record::Txt(text) => text.parse().ok(),
            Record::A(_) => None,
        })
        .ok_or_else(|| "public IP lookup returned no address".to_string())
}

/// Caches lookups, since routes to different regions share most hops.
#[derive(Default)]
pub struct Lookup {
    cache: Mutex<HashMap<Ipv4Addr, Option<NetworkInfo>>>,
}

impl Lookup {
    /// `None` for private addresses and addresses no network announces.
    pub fn network(&self, ip: Ipv4Addr) -> Option<NetworkInfo> {
        if !is_public(ip) {
            return None;
        }
        if let Some(hit) = self.cache.lock().ok()?.get(&ip) {
            return hit.clone();
        }
        let info = match lookup_asn(ip) {
            Ok(info) => info,
            Err(e) => {
                tracing::debug!(%ip, error = %e, "ASN lookup failed");
                None
            }
        };
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(ip, info.clone());
        }
        info
    }
}

fn lookup_asn(ip: Ipv4Addr) -> Result<Option<NetworkInfo>, String> {
    let [a, b, c, d] = ip.octets();
    let origin = txt(&format!("{d}.{c}.{b}.{a}.origin.asn.cymru.com"))?;
    let Some(mut info) = origin.iter().find_map(|t| parse_origin(t)) else {
        return Ok(None);
    };
    if let Ok(names) = txt(&format!("AS{}.asn.cymru.com", info.asn)) {
        info.as_name = names.iter().find_map(|t| parse_as_name(t));
    }
    Ok(Some(info))
}

fn txt(name: &str) -> Result<Vec<String>, String> {
    let records = dns::resolve(name, QueryType::Txt).map_err(|e| e.to_string())?;
    Ok(records
        .into_iter()
        .filter_map(|r| match r {
            Record::Txt(t) => Some(t),
            Record::A(_) => None,
        })
        .collect())
}

/// `"13335 | 1.1.1.0/24 | AU | apnic | 2011-08-11"`; several origin ASNs
/// may be listed, space-separated, in the first field.
fn parse_origin(text: &str) -> Option<NetworkInfo> {
    let fields: Vec<&str> = text.split('|').map(str::trim).collect();
    let asn = fields.first()?.split_whitespace().next()?.parse().ok()?;
    let non_empty = |i: usize| {
        fields
            .get(i)
            .filter(|f| !f.is_empty())
            .map(|f| f.to_string())
    };
    Some(NetworkInfo {
        asn,
        as_name: None,
        prefix: non_empty(1),
        country: non_empty(2),
    })
}

/// `"13335 | US | arin | 2010-07-14 | CLOUDFLARENET, US"`
fn parse_as_name(text: &str) -> Option<String> {
    text.split('|')
        .nth(4)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

pub fn is_public(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    let cgnat = a == 100 && (b & 0xc0) == 64;
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || cgnat)
}

/// Shows addresses in reports the way the diagnostics crate redacts them.
pub fn redacted(ip: IpAddr) -> String {
    pingpath_diagnostics::redact_ip(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cymru_answers() {
        assert_eq!(
            parse_origin("13335 209242 | 1.1.1.0/24 | AU | apnic | 2011-08-11"),
            Some(NetworkInfo {
                asn: 13335,
                as_name: None,
                prefix: Some("1.1.1.0/24".into()),
                country: Some("AU".into()),
            })
        );
        assert_eq!(
            parse_as_name("13335 | US | arin | 2010-07-14 | CLOUDFLARENET, US"),
            Some("CLOUDFLARENET, US".into())
        );
        assert_eq!(parse_origin("garbage"), None);
    }

    #[test]
    fn private_and_cgnat_addresses_are_not_public() {
        assert!(!is_public(Ipv4Addr::new(192, 168, 1, 1)));
        assert!(!is_public(Ipv4Addr::new(100, 70, 0, 1)));
        assert!(is_public(Ipv4Addr::new(8, 8, 8, 8)));
    }
}
