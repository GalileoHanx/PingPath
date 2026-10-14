//! The doctor's system readings on Linux, for development and the tests:
//! the default route's interface from /proc/net/route, what it is from
//! /sys/class/net, and the Wi-Fi signal from /proc/net/wireless. The band
//! isn't read here (it needs nl80211).

use std::fs;
use std::io;
use std::path::Path;

use super::connection::{Figures, Medium, WifiFigures};

/// ARPHRD_ETHER in /sys/class/net/<name>/type: Ethernet and Wi-Fi alike.
const ARPHRD_ETHER: u32 = 1;

pub fn connection() -> io::Result<Figures> {
    let routes = fs::read_to_string("/proc/net/route")?;
    let name = default_interface(&routes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no default IPv4 route"))?;
    let sys = Path::new("/sys/class/net").join(&name);
    let read = |file: &str| fs::read_to_string(sys.join(file)).ok();
    let wireless = sys.join("wireless").exists() || sys.join("phy80211").exists();
    // Physical cards have a device behind them; veth, tun, bridges don't.
    let physical = sys.join("device").exists();
    let ether = read("type").and_then(|t| t.trim().parse::<u32>().ok()) == Some(ARPHRD_ETHER);
    let medium = if wireless {
        Medium::Wifi
    } else if physical && ether {
        Medium::Ethernet
    } else {
        Medium::Other
    };
    // -1 (or an error) when the driver doesn't say, as Wi-Fi drivers don't.
    let link_mbps = read("speed")
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|s| *s > 0)
        .map(|s| s as f64);
    let wifi = wireless.then(|| {
        fs::read_to_string("/proc/net/wireless")
            .ok()
            .and_then(|text| wireless_signal(&text, &name))
            .unwrap_or_default()
    });
    Ok(Figures {
        medium,
        adapter: Some(name),
        is_virtual: !physical,
        link_mbps,
        wifi,
    })
}

/// The interface of the default IPv4 route with the lowest metric.
fn default_interface(routes: &str) -> Option<String> {
    routes
        .lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            // Iface Destination Gateway Flags RefCnt Use Metric Mask ...
            let up = u32::from_str_radix(cols.get(3)?, 16).ok()? & 1 != 0;
            let default = *cols.get(1)? == "00000000" && *cols.get(7)? == "00000000";
            if !up || !default {
                return None;
            }
            Some((cols.get(6)?.parse::<u32>().ok()?, cols[0].to_string()))
        })
        .min()
        .map(|(_, name)| name)
}

/// The signal level (dBm) and link quality of `name` in /proc/net/wireless.
/// Quality is out of 70 for most drivers.
fn wireless_signal(text: &str, name: &str) -> Option<WifiFigures> {
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with(&format!("{name}:")))?;
    let cols: Vec<&str> = line.split(':').nth(1)?.split_whitespace().collect();
    let number = |i: usize| cols.get(i)?.trim_end_matches('.').parse::<f64>().ok();
    let quality = number(1).map(|q| ((q / 70.0) * 100.0).round().clamp(0.0, 100.0) as u32);
    // Some drivers report the level as an unsigned byte.
    let level = number(2).map(|l| if l > 0.0 { l - 256.0 } else { l } as i32);
    Some(WifiFigures {
        rssi_dbm: level.filter(|l| (-120..0).contains(l)),
        quality_pct: quality,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_default_route_with_the_lowest_metric() {
        let routes = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
            wlp2s0\t00000000\t0102A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
            enp3s0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
            enp3s0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(default_interface(routes).as_deref(), Some("enp3s0"));
        let only_local = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
            enp3s0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\n";
        assert_eq!(default_interface(only_local), None);
    }

    /// This machine's own default route, whatever it is.
    #[test]
    fn reads_this_machines_connection() {
        match connection() {
            Ok(figures) => assert!(figures.adapter.is_some_and(|name| !name.is_empty())),
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}"),
        }
    }

    #[test]
    fn reads_the_signal_and_quality() {
        let text = "Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE\n \
            face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22\n\
            wlp2s0: 0000   49.  -61.  -256        0      0      0      0     15        0\n";
        let wifi = wireless_signal(text, "wlp2s0").unwrap();
        assert_eq!(wifi.rssi_dbm, Some(-61));
        assert_eq!(wifi.quality_pct, Some(70));
        assert!(wireless_signal(text, "wlan1").is_none());
        let unsigned = "wlan0: 0000   35.  190.  0  0 0 0 0 0 0\n";
        assert_eq!(
            wireless_signal(unsigned, "wlan0").unwrap().rssi_dbm,
            Some(-66)
        );
    }
}
