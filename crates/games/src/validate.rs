use std::collections::HashSet;
use std::sync::LazyLock;

use ipnet::IpNet;
use jsonschema::Validator;
use serde_json::Value;

use crate::model::{GameDefinition, RoutingPolicy, SupportStatus};

/// The JSON Schema every game definition must satisfy.
pub const SCHEMA: &str = include_str!("../../../games/schema.json");

/// Most UDP ports a game may list in total.
const MAX_UDP_PORTS: u32 = 2048;

/// UDP services commonly abused for reflection and amplification attacks.
/// No game uses these, and an exit relay must never forward to them.
const REFLECTION_PORTS: &[(u16, &str)] = &[
    (7, "echo"),
    (17, "quote of the day"),
    (19, "chargen"),
    (53, "DNS"),
    (69, "TFTP"),
    (111, "portmap"),
    (123, "NTP"),
    (137, "NetBIOS name"),
    (138, "NetBIOS datagram"),
    (161, "SNMP"),
    (162, "SNMP trap"),
    (389, "CLDAP"),
    (520, "RIP"),
    (1900, "SSDP"),
    (3702, "WS-Discovery"),
    (5353, "mDNS"),
    (11211, "memcached"),
];

/// Broadest prefixes a game may route. Anything wider would pull unrelated
/// traffic into the tunnel.
const MIN_PREFIX_V4: u8 = 16;
const MIN_PREFIX_V6: u8 = 32;

static VALIDATOR: LazyLock<Validator> = LazyLock::new(|| {
    let schema: Value = serde_json::from_str(SCHEMA).expect("games/schema.json is valid JSON");
    jsonschema::validator_for(&schema).expect("games/schema.json is a valid JSON Schema")
});

/// Address space that must never be routed through the tunnel: sending it
/// there would break the player's LAN or make no sense on the internet.
static NON_PUBLIC: LazyLock<Vec<(IpNet, &'static str)>> = LazyLock::new(|| {
    [
        ("0.0.0.0/8", "\"this network\""),
        ("10.0.0.0/8", "private"),
        ("100.64.0.0/10", "carrier-grade NAT"),
        ("127.0.0.0/8", "loopback"),
        ("169.254.0.0/16", "link-local"),
        ("172.16.0.0/12", "private"),
        ("192.0.0.0/24", "IETF protocol assignment"),
        ("192.168.0.0/16", "private"),
        ("198.18.0.0/15", "benchmarking"),
        ("224.0.0.0/4", "multicast"),
        ("240.0.0.0/4", "reserved"),
    ]
    .into_iter()
    .map(|(net, name)| (net.parse().expect("valid CIDR"), name))
    .collect()
});

/// IPv6 outside this block (unique local, link-local, multicast, loopback,
/// IPv4-mapped, ...) is not globally routable unicast.
static GLOBAL_UNICAST_V6: LazyLock<IpNet> = LazyLock::new(|| "2000::/3".parse().unwrap());

/// Documentation ranges (RFC 5737, RFC 3849, RFC 9637). Allowed only as
/// placeholders in definitions the client refuses to route (`untested`,
/// `unsupported`), so fake addresses never get routed.
static DOCUMENTATION: LazyLock<Vec<IpNet>> = LazyLock::new(|| {
    [
        "192.0.2.0/24",
        "198.51.100.0/24",
        "203.0.113.0/24",
        "2001:db8::/32",
        "3fff::/20",
    ]
    .into_iter()
    .map(|net| net.parse().expect("valid CIDR"))
    .collect()
});

/// Parses and validates one game definition from YAML source.
///
/// Checks run in order: YAML syntax, the JSON Schema, then rules the schema
/// cannot express. Every problem found at the failing stage is returned.
/// File-level rules (file name, report file) are checked by
/// [`crate::load_file`].
pub fn parse_definition(source: &str) -> Result<GameDefinition, Vec<String>> {
    let value: Value =
        serde_yaml_ng::from_str(source).map_err(|e| vec![format!("invalid YAML: {e}")])?;

    let schema_problems: Vec<String> = VALIDATOR
        .iter_errors(&value)
        .map(|e| {
            let path = e.instance_path().to_string();
            if path.is_empty() {
                e.to_string()
            } else {
                format!("{path}: {e}")
            }
        })
        .collect();
    if !schema_problems.is_empty() {
        return Err(schema_problems);
    }

    let definition: GameDefinition = serde_path_to_error::deserialize(&value)
        .map_err(|e| vec![format!("{}: {}", e.path(), e.inner())])?;

    let problems = check_rules(&definition);
    if problems.is_empty() {
        Ok(definition)
    } else {
        Err(problems)
    }
}

fn check_rules(def: &GameDefinition) -> Vec<String> {
    let mut problems = Vec::new();
    let placeholders_allowed = def.status.routing_policy() == RoutingPolicy::Refuse;

    // The exit forwards to these ports, so they must look like game ports,
    // not services that can be abused for reflection attacks.
    let udp_ports: u32 = def
        .ports
        .udp
        .iter()
        .map(|r| u32::from(r.last - r.first) + 1)
        .sum();
    if udp_ports > MAX_UDP_PORTS {
        problems.push(format!(
            "ports.udp: {udp_ports} ports listed; at most {MAX_UDP_PORTS} (list the game's actual ports)"
        ));
    }
    for (port, service) in REFLECTION_PORTS {
        if def.ports.udp.iter().any(|r| r.contains(*port)) {
            problems.push(format!(
                "ports.udp: includes {port} ({service}), a port abused for reflection attacks"
            ));
        }
    }

    if let Some(report) = &def.report {
        let expected = format!("docs/anticheat/{}.md", def.id);
        if *report != expected {
            problems.push(format!(
                "report: expected \"{expected}\", found \"{report}\""
            ));
        }
    }

    let mut region_ids = HashSet::new();
    let mut seen: Vec<(&str, IpNet)> = Vec::new();
    for region in &def.regions {
        let at = format!("region \"{}\"", region.id);
        if !region_ids.insert(region.id.as_str()) {
            problems.push(format!("{at}: duplicate region id"));
        }

        for net in &region.server_ranges {
            if *net != net.trunc() {
                problems.push(format!(
                    "{at}: {net} has host bits set; did you mean {}?",
                    net.trunc()
                ));
            }
            let min = match net {
                IpNet::V4(_) => MIN_PREFIX_V4,
                IpNet::V6(_) => MIN_PREFIX_V6,
            };
            if net.prefix_len() < min {
                problems.push(format!(
                    "{at}: {net} is broader than /{min}; list the game's actual server prefixes"
                ));
            }
            if let Some(problem) = address_problem(*net, def.status, placeholders_allowed) {
                problems.push(format!("{at}: {problem}"));
            }
            for (other_region, other) in &seen {
                if net.contains(other) || other.contains(net) {
                    problems.push(format!(
                        "{at}: {net} overlaps {other} in region \"{other_region}\""
                    ));
                }
            }
            seen.push((&region.id, *net));
        }

        for target in &region.ping_targets {
            if let Some(problem) =
                address_problem(IpNet::from(target.ip()), def.status, placeholders_allowed)
            {
                problems.push(format!("{at}: ping target {problem}"));
            }
        }
        if !region.sdr_pops.is_empty() && def.sdr_app_id.is_none() {
            problems.push(format!(
                "{at}: sdr_pops needs sdr_app_id at the top of the file"
            ));
        }
    }

    problems
}

/// Explains why `net` may not appear in a definition with this status.
fn address_problem(
    net: IpNet,
    status: SupportStatus,
    placeholders_allowed: bool,
) -> Option<String> {
    let overlaps = |other: &IpNet| net.contains(other) || other.contains(&net);
    let shown = display(net);

    if let IpNet::V6(_) = net {
        if !GLOBAL_UNICAST_V6.contains(&net) {
            return Some(format!(
                "{shown} is not global unicast address space and must never be tunneled"
            ));
        }
    }
    if let Some((_, name)) = NON_PUBLIC.iter().find(|(other, _)| overlaps(other)) {
        return Some(format!(
            "{shown} overlaps {name} address space, which must never be tunneled"
        ));
    }
    if !placeholders_allowed && DOCUMENTATION.iter().any(overlaps) {
        return Some(format!(
            "{shown} is a documentation (placeholder) range; replace it with verified addresses \
             before setting status to {status}"
        ));
    }
    None
}

/// Shows single addresses without a /32 or /128 suffix.
fn display(net: IpNet) -> String {
    if net.prefix_len() == net.max_prefix_len() {
        net.addr().to_string()
    } else {
        net.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problem(net: &str, status: SupportStatus) -> Option<String> {
        let placeholders_allowed = status.routing_policy() == RoutingPolicy::Refuse;
        address_problem(net.parse().unwrap(), status, placeholders_allowed)
    }

    #[test]
    fn public_ranges_are_accepted() {
        assert_eq!(problem("1.1.1.0/24", SupportStatus::Supported), None);
        assert_eq!(problem("2606:4700::/32", SupportStatus::Supported), None);
    }

    #[test]
    fn non_public_ranges_are_rejected_for_every_status() {
        for net in [
            "10.1.0.0/16",
            "192.168.1.0/24",
            "100.64.0.0/16",
            "127.0.0.1/32",
            "224.0.0.0/24",
        ] {
            assert!(problem(net, SupportStatus::Untested).is_some(), "{net}");
        }
        for net in ["fd00::/32", "fe80::/64", "ff02::/16", "::1/128"] {
            assert!(problem(net, SupportStatus::Untested).is_some(), "{net}");
        }
    }

    #[test]
    fn a_range_containing_private_space_is_rejected() {
        assert!(
            problem("172.0.0.0/8", SupportStatus::Supported)
                .unwrap()
                .contains("private")
        );
    }

    #[test]
    fn placeholder_ranges_only_in_definitions_that_are_never_routed() {
        assert_eq!(problem("203.0.113.0/24", SupportStatus::Untested), None);
        assert_eq!(problem("2001:db8:1::/48", SupportStatus::Unsupported), None);
        for status in [SupportStatus::Testing, SupportStatus::Supported] {
            let p = problem("198.51.100.0/24", status).unwrap();
            assert!(p.contains("placeholder"), "{p}");
        }
    }

    #[test]
    fn single_addresses_are_shown_without_prefix() {
        let p = address_problem(
            "10.0.0.1/32".parse().unwrap(),
            SupportStatus::Untested,
            true,
        );
        assert!(p.unwrap().starts_with("10.0.0.1 "));
    }
}
