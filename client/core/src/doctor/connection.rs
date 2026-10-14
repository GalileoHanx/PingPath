//! Wi-Fi or Ethernet, signal strength, and the Wi-Fi band
//! (docs/decisions/011-home-network-doctor.md, "Connection").

use std::io;

use serde::{Deserialize, Serialize};

use super::{CheckId, CheckResult, Finding, FindingCode, Severity};

/// Below this the signal is weak enough to cost packets now and then.
pub const WEAK_SIGNAL_DBM: i32 = -67;
/// Below this it costs them often.
pub const POOR_SIGNAL_DBM: i32 = -75;
/// A cable link slower than this usually means a damaged cable or a port
/// stuck at 10 Mbit/s.
pub const SLOW_LINK_MBPS: f64 = 100.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Medium {
    Ethernet,
    Wifi,
    /// Neither, or not known (a virtual adapter, a mobile modem).
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Band {
    #[serde(rename = "2.4")]
    Ghz2_4,
    #[serde(rename = "5")]
    Ghz5,
    #[serde(rename = "6")]
    Ghz6,
}

impl Band {
    /// From the access point's centre frequency in MHz.
    pub fn from_mhz(mhz: u32) -> Option<Self> {
        match mhz {
            2400..=2500 => Some(Self::Ghz2_4),
            4900..=5924 => Some(Self::Ghz5),
            5925..=7125 => Some(Self::Ghz6),
            _ => None,
        }
    }

    /// From the channel number alone, when the frequency isn't known.
    /// 6 GHz channels reuse the same numbers, so a Wi-Fi 6 or 7 link on
    /// channels 1 to 14 is left unknown rather than called 2.4 GHz.
    pub fn from_channel(channel: u32, generation: Option<u8>) -> Option<Self> {
        match channel {
            1..=14 if generation.is_none_or(|g| g < 6) => Some(Self::Ghz2_4),
            32..=177 => Some(Self::Ghz5),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WifiFigures {
    /// Received signal strength.
    pub rssi_dbm: Option<i32>,
    /// The system's 0 to 100 signal quality.
    pub quality_pct: Option<u32>,
    pub band: Option<Band>,
    pub channel: Option<u32>,
    /// Wi-Fi 4 to 7 (802.11n, ac, ax, be); none for older ones.
    pub generation: Option<u8>,
    /// The link's current rates.
    pub rx_mbps: Option<f64>,
    pub tx_mbps: Option<f64>,
}

impl WifiFigures {
    /// The signal in dBm: measured, or from the 0 to 100 quality, which
    /// Windows maps linearly from -100 dBm (0) to -50 dBm (100).
    pub fn signal_dbm(&self) -> Option<i32> {
        self.rssi_dbm
            .or_else(|| self.quality_pct.map(|q| q.min(100) as i32 / 2 - 100))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Figures {
    pub medium: Medium,
    /// The adapter's name, as the system shows it.
    pub adapter: Option<String>,
    /// Not a physical network card: another VPN, a virtual switch.
    #[serde(rename = "virtual")]
    pub is_virtual: bool,
    /// The negotiated link speed, receiving.
    pub link_mbps: Option<f64>,
    /// Wi-Fi only, when the system says.
    pub wifi: Option<WifiFigures>,
}

pub fn findings(f: &Figures) -> Vec<Finding> {
    let mut out = Vec::new();
    if f.is_virtual {
        out.push(
            Finding::new(FindingCode::VirtualAdapter, Severity::Warning)
                .with("adapter", f.adapter.clone().unwrap_or_default()),
        );
    }
    match f.medium {
        Medium::Ethernet => {
            out.push(Finding::new(FindingCode::Ethernet, Severity::Info));
            if let Some(mbps) = f.link_mbps.filter(|m| *m < SLOW_LINK_MBPS) {
                out.push(
                    Finding::new(FindingCode::SlowLink, Severity::Warning)
                        .with("mbps", mbps.round()),
                );
            }
        }
        Medium::Wifi => {
            out.push(Finding::new(FindingCode::Wifi, Severity::Warning));
            let wifi = f.wifi.clone().unwrap_or_default();
            if let Some(dbm) = wifi.signal_dbm() {
                let severity = if dbm < POOR_SIGNAL_DBM {
                    Some(Severity::Problem)
                } else if dbm < WEAK_SIGNAL_DBM {
                    Some(Severity::Warning)
                } else {
                    None
                };
                if let Some(severity) = severity {
                    out.push(Finding::new(FindingCode::WeakSignal, severity).with("rssi_dbm", dbm));
                }
            }
            if wifi.band == Some(Band::Ghz2_4) {
                out.push(Finding::new(FindingCode::CrowdedBand, Severity::Warning));
            }
        }
        Medium::Other => {}
    }
    out
}

pub fn check(read: io::Result<Figures>) -> CheckResult {
    match read {
        Ok(figures) => {
            let found = findings(&figures);
            CheckResult::from_findings(CheckId::Connection, &figures, found)
        }
        Err(e) => CheckResult::from_error(CheckId::Connection, &e),
    }
}

/// Reads the connection that carries traffic to the internet.
pub fn read() -> io::Result<Figures> {
    #[cfg(windows)]
    return super::windows::connection();
    #[cfg(target_os = "linux")]
    return super::linux::connection();
    #[cfg(not(any(windows, target_os = "linux")))]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the connection check runs on Windows and Linux",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Status;

    fn wifi(wifi: WifiFigures) -> Figures {
        Figures {
            medium: Medium::Wifi,
            adapter: Some("Wi-Fi".into()),
            is_virtual: false,
            link_mbps: Some(866.0),
            wifi: Some(wifi),
        }
    }

    fn codes(f: &Figures) -> Vec<(FindingCode, Severity)> {
        findings(f).iter().map(|f| (f.code, f.severity)).collect()
    }

    #[test]
    fn a_cable_is_good_unless_it_runs_slow() {
        let mut cable = Figures {
            medium: Medium::Ethernet,
            adapter: Some("Ethernet".into()),
            is_virtual: false,
            link_mbps: Some(1000.0),
            wifi: None,
        };
        assert_eq!(check(Ok(cable.clone())).status, Status::Good);
        cable.link_mbps = Some(10.0);
        let found = findings(&cable);
        assert_eq!(found[1].code, FindingCode::SlowLink);
        assert_eq!(found[1].params["mbps"], 10.0);
        assert_eq!(check(Ok(cable)).status, Status::Warning);
    }

    #[test]
    fn wifi_is_a_warning_and_a_weak_signal_or_crowded_band_more() {
        let strong = WifiFigures {
            rssi_dbm: Some(-50),
            band: Some(Band::Ghz5),
            ..Default::default()
        };
        assert_eq!(
            codes(&wifi(strong.clone())),
            [(FindingCode::Wifi, Severity::Warning)]
        );
        let weak = WifiFigures {
            rssi_dbm: Some(-70),
            ..strong.clone()
        };
        assert_eq!(
            codes(&wifi(weak))[1],
            (FindingCode::WeakSignal, Severity::Warning)
        );
        let poor = WifiFigures {
            rssi_dbm: Some(-80),
            band: Some(Band::Ghz2_4),
            ..strong
        };
        let found = findings(&wifi(poor.clone()));
        assert_eq!(
            (found[1].code, found[1].severity),
            (FindingCode::WeakSignal, Severity::Problem)
        );
        assert_eq!(found[1].params["rssi_dbm"], -80);
        assert_eq!(found[2].code, FindingCode::CrowdedBand);
        assert_eq!(check(Ok(wifi(poor))).status, Status::Problem);
    }

    #[test]
    fn the_quality_stands_in_for_a_missing_signal_strength() {
        let quality = |q| WifiFigures {
            quality_pct: Some(q),
            ..Default::default()
        };
        assert_eq!(quality(100).signal_dbm(), Some(-50));
        assert_eq!(quality(60).signal_dbm(), Some(-70));
        assert_eq!(quality(0).signal_dbm(), Some(-100));
        assert_eq!(
            codes(&wifi(quality(40)))[1],
            (FindingCode::WeakSignal, Severity::Problem)
        );
        // No figures at all: just "on Wi-Fi".
        assert_eq!(codes(&wifi(WifiFigures::default())).len(), 1);
    }

    #[test]
    fn a_virtual_adapter_is_named() {
        let tunnel = Figures {
            medium: Medium::Other,
            adapter: Some("SomeVPN Tunnel".into()),
            is_virtual: true,
            link_mbps: None,
            wifi: None,
        };
        let found = findings(&tunnel);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].code, FindingCode::VirtualAdapter);
        assert_eq!(found[0].params["adapter"], "SomeVPN Tunnel");
    }

    #[test]
    fn bands_from_frequency_or_channel() {
        assert_eq!(Band::from_mhz(2437), Some(Band::Ghz2_4));
        assert_eq!(Band::from_mhz(5180), Some(Band::Ghz5));
        assert_eq!(Band::from_mhz(5955), Some(Band::Ghz6));
        assert_eq!(Band::from_mhz(60_480), None);
        assert_eq!(Band::from_channel(6, Some(5)), Some(Band::Ghz2_4));
        assert_eq!(Band::from_channel(6, None), Some(Band::Ghz2_4));
        assert_eq!(Band::from_channel(5, Some(6)), None);
        assert_eq!(Band::from_channel(36, Some(6)), Some(Band::Ghz5));
        assert_eq!(Band::from_channel(200, None), None);
    }

    #[test]
    fn figures_go_over_the_pipe_with_plain_names() {
        let figures = wifi(WifiFigures {
            rssi_dbm: Some(-55),
            band: Some(Band::Ghz2_4),
            generation: Some(6),
            ..Default::default()
        });
        let value = serde_json::to_value(&figures).unwrap();
        assert_eq!(value["medium"], "wifi");
        assert_eq!(value["virtual"], false);
        assert_eq!(value["wifi"]["band"], "2.4");
        assert_eq!(value["wifi"]["generation"], 6);
    }
}
