//! Interface and route changes through iproute2's `ip` (blocking, like
//! the `Platform` trait). Every route PingPath adds carries `proto 80`, so
//! cleanup removes exactly its own; Linux is for development and tests, so
//! a protocol number is enough here, unlike Windows (docs/decisions/008).

use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::process::Command;

use ipnet::IpNet;
use serde_json::Value;

/// Route protocol marking PingPath's routes. Any value not listed in
/// /etc/iproute2/rt_protos works; 80 is unused there.
pub const PROTO: u8 = 80;

/// Why `ip` failed: its exit status and stderr (in the C locale).
fn ip_error(args: &[&str], stderr: &str) -> io::Error {
    let kind = if stderr.contains("File exists") {
        io::ErrorKind::AlreadyExists
    } else if stderr.contains("No such process")
        || stderr.contains("Cannot find device")
        || stderr.contains("does not exist")
    {
        io::ErrorKind::NotFound
    } else if stderr.contains("Operation not permitted") {
        io::ErrorKind::PermissionDenied
    } else {
        io::ErrorKind::Other
    };
    io::Error::new(kind, format!("ip {}: {}", args.join(" "), stderr.trim()))
}

/// Runs `ip <args>` and returns stdout. LC_ALL=C keeps error text stable
/// for the matching in `ip_error`. Blocking (fork + exec, ~1-3 ms).
pub fn ip(args: &[&str]) -> io::Result<String> {
    let out = Command::new("ip").args(args).env("LC_ALL", "C").output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(ip_error(args, &String::from_utf8_lossy(&out.stderr)))
    }
}

/// Gives the adapter its address and MTU and brings it up.
pub fn configure_adapter(name: &str, address: Ipv4Addr, mtu: u16) -> io::Result<()> {
    ip(&["addr", "add", &format!("{address}/32"), "dev", name])?;
    ip(&["link", "set", "dev", name, "mtu", &mtu.to_string(), "up"])?;
    Ok(())
}

/// Interface index (InterfaceId on Linux) <-> name, via if_nametoindex(3)
/// and if_indextoname(3) in nix's "net" feature.
pub fn index_of(name: &str) -> io::Result<u32> {
    nix::net::if_::if_nametoindex(name).map_err(io::Error::from)
}

pub fn name_of(index: u32) -> io::Result<String> {
    let name = nix::net::if_::if_indextoname(index).map_err(io::Error::from)?;
    Ok(name.to_string_lossy().into_owned())
}

/// How the system reaches `dst`, ignoring PingPath's own routes, so a
/// relay's host route doesn't hide a change of network: the kernel's
/// answer (`ip route get fibmatch`, which follows policy rules) when the
/// route it matched isn't ours, otherwise the best other route in the
/// main table.
pub fn physical_route(dst: IpAddr, adapter: &str) -> io::Result<(Option<IpAddr>, String)> {
    let matched = parse_routes(
        &ip(&["-j", "route", "get", "fibmatch", &dst.to_string()])?,
        dst.is_ipv6(),
    )?;
    if let Some(next_hop) = matched
        .first()
        .filter(|route| !route.ours(adapter))
        .and_then(RouteRow::next_hop)
    {
        return Ok(next_hop);
    }
    let family = if dst.is_ipv6() { "-6" } else { "-4" };
    let table = parse_routes(
        &ip(&[family, "-j", "route", "show", "table", "main"])?,
        dst.is_ipv6(),
    )?;
    choose(&table, dst, adapter)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no route to {dst}")))
}

/// One row of `ip -j route` output.
#[derive(Debug, Clone, PartialEq)]
struct RouteRow {
    dst: IpNet,
    gateway: Option<IpAddr>,
    dev: Option<String>,
    protocol: Option<String>,
    metric: u32,
    /// `unicast` when absent; `blackhole`, `local` and others can't be used.
    kind: Option<String>,
}

impl RouteRow {
    fn ours(&self, adapter: &str) -> bool {
        self.protocol.as_deref() == Some(PROTO.to_string().as_str())
            || self.dev.as_deref() == Some(adapter)
    }

    fn next_hop(&self) -> Option<(Option<IpAddr>, String)> {
        let unicast = self.kind.as_deref().is_none_or(|k| k == "unicast");
        unicast
            .then(|| self.dev.clone().map(|dev| (self.gateway, dev)))
            .flatten()
    }
}

fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

fn parse_routes(json: &str, ipv6: bool) -> io::Result<Vec<RouteRow>> {
    let json = if json.trim().is_empty() { "[]" } else { json };
    let rows: Vec<Value> = serde_json::from_str(json).map_err(invalid)?;
    let text = |row: &Value, key: &str| row.get(key).and_then(Value::as_str).map(str::to_string);
    rows.iter()
        .map(|row| {
            let dst = match text(row, "dst").as_deref() {
                Some("default") | None if ipv6 => "::/0".parse().map_err(invalid)?,
                Some("default") | None => "0.0.0.0/0".parse().map_err(invalid)?,
                Some(dst) if dst.contains('/') => dst.parse().map_err(invalid)?,
                Some(dst) => IpNet::from(dst.parse::<IpAddr>().map_err(invalid)?),
            };
            Ok(RouteRow {
                dst,
                gateway: text(row, "gateway")
                    .map(|g| g.parse().map_err(invalid))
                    .transpose()?,
                dev: text(row, "dev"),
                protocol: text(row, "protocol"),
                metric: row
                    .get("metric")
                    .and_then(Value::as_u64)
                    .map_or(0, |m| m as u32),
                kind: text(row, "type"),
            })
        })
        .collect()
}

/// The longest matching prefix that isn't ours, then the lowest metric.
fn choose(table: &[RouteRow], dst: IpAddr, adapter: &str) -> Option<(Option<IpAddr>, String)> {
    table
        .iter()
        .filter(|route| !route.ours(adapter) && route.dst.contains(&dst))
        .filter_map(|route| route.next_hop().map(|hop| (route, hop)))
        .max_by_key(|(route, _)| (route.dst.prefix_len(), std::cmp::Reverse(route.metric)))
        .map(|(_, hop)| hop)
}

/// `ip route add <prefix> [via <gw>] dev <dev> proto 80`. Adding a route
/// that already exists *and is ours* succeeds (idempotent like the fake);
/// a clashing route that is not ours stays an `AlreadyExists` error.
/// `ip` picks `-4`/`-6` from the prefix; mixing families with `via` fails.
pub fn route_add(prefix: IpNet, gateway: Option<IpAddr>, dev: &str) -> io::Result<()> {
    let prefix_s = prefix.to_string();
    let proto = PROTO.to_string();
    let gw_s = gateway.map(|g| g.to_string());
    let mut args = vec!["route", "add", prefix_s.as_str()];
    if let Some(gw) = gw_s.as_deref() {
        args.extend(["via", gw]);
    }
    args.extend(["dev", dev, "proto", proto.as_str()]);
    match ip(&args) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && is_ours(prefix, dev)? => Ok(()),
        other => other.map(drop),
    }
}

fn is_ours(prefix: IpNet, dev: &str) -> io::Result<bool> {
    let family = if prefix.addr().is_ipv4() { "-4" } else { "-6" };
    let out = ip(&[
        family,
        "-j",
        "route",
        "show",
        "exact",
        &prefix.to_string(),
        "dev",
        dev,
        "proto",
        &PROTO.to_string(),
    ])?;
    Ok(out.trim() != "[]" && !out.trim().is_empty())
}

/// `ip route del <prefix> dev <dev> proto 80`; gone already is Ok.
/// `proto` makes the kernel match only our route, never a user's.
pub fn route_del(prefix: IpNet, dev: &str) -> io::Result<()> {
    match ip(&[
        "route",
        "del",
        &prefix.to_string(),
        "dev",
        dev,
        "proto",
        &PROTO.to_string(),
    ]) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other.map(drop),
    }
}

/// Deletes every route with our protocol in both families; returns how
/// many there were. (`ip route flush` alone prints nothing countable.)
pub fn flush_ours() -> io::Result<usize> {
    let proto = PROTO.to_string();
    let mut count = 0;
    for family in ["-4", "-6"] {
        let out = ip(&[family, "-j", "route", "show", "proto", &proto])?;
        let rows: Value = serde_json::from_str(if out.trim().is_empty() { "[]" } else { &out })
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let n = rows.as_array().map_or(0, Vec::len);
        if n > 0 {
            ip(&[family, "route", "flush", "proto", &proto])?;
        }
        count += n;
    }
    Ok(count)
}

/// Deletes a stale interface left by a crash (`ip link del <name>`);
/// returns whether there was one.
pub fn delete_link(name: &str) -> io::Result<bool> {
    match ip(&["link", "del", "dev", name]) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = r#"[
        {"dst":"default","gateway":"10.1.0.1","dev":"g0","flags":[]},
        {"dst":"default","gateway":"10.9.0.1","dev":"wlan0","metric":600,"flags":[]},
        {"dst":"10.1.0.0/24","dev":"g0","protocol":"kernel","scope":"link","prefsrc":"10.1.0.2","flags":[]},
        {"dst":"10.2.1.11","gateway":"10.1.0.1","dev":"g0","protocol":"80","flags":[]},
        {"dst":"10.3.1.0/24","dev":"PingPath","protocol":"80","scope":"link","flags":[]},
        {"type":"blackhole","dst":"10.4.0.0/16","flags":[]}
    ]"#;

    #[test]
    fn parses_ip_json_including_default_and_host_routes() {
        let rows = parse_routes(TABLE, false).unwrap();
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0].dst, "0.0.0.0/0".parse::<IpNet>().unwrap());
        assert_eq!(rows[1].metric, 600);
        assert_eq!(rows[3].dst, "10.2.1.11/32".parse::<IpNet>().unwrap());
        assert!(rows[3].ours("PingPath") && rows[4].ours("PingPath"));
        assert!(!rows[2].ours("PingPath"));
        assert_eq!(rows[5].next_hop(), None);
        assert_eq!(parse_routes("", true).unwrap(), vec![]);
        let v6 = parse_routes(
            r#"[{"dst":"default","gateway":"fe80::1","dev":"g0"}]"#,
            true,
        )
        .unwrap();
        assert_eq!(v6[0].dst, "::/0".parse::<IpNet>().unwrap());
    }

    #[test]
    fn the_physical_route_ignores_pingpaths_own() {
        let table = parse_routes(TABLE, false).unwrap();
        let gateway = |s: &str| Some(s.parse::<IpAddr>().unwrap());
        // A relay with a PingPath host route: the default route it replaced.
        assert_eq!(
            choose(&table, "10.2.1.11".parse().unwrap(), "PingPath"),
            Some((gateway("10.1.0.1"), "g0".to_string()))
        );
        // A game server routed to the adapter: still the physical way.
        assert_eq!(
            choose(&table, "10.3.1.10".parse().unwrap(), "PingPath"),
            Some((gateway("10.1.0.1"), "g0".to_string()))
        );
        // On-link: no gateway.
        assert_eq!(
            choose(&table, "10.1.0.7".parse().unwrap(), "PingPath"),
            Some((None, "g0".to_string()))
        );
        // A blackhole is no way out; the default route is.
        assert_eq!(
            choose(&table, "10.4.0.1".parse().unwrap(), "PingPath"),
            Some((gateway("10.1.0.1"), "g0".to_string()))
        );
    }
}
