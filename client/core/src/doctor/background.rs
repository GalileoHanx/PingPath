//! Processes using the network in the background
//! (docs/decisions/011-home-network-doctor.md, "Background traffic"): what
//! each process sent and received over a few seconds, from the kernel's
//! own network events on Windows. Nothing is hooked or diverted (rules 4
//! and 5); the check only reads events Windows already produces.

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{CheckId, CheckResult, Finding, FindingCode, Severity};

/// How long the check watches.
pub const WINDOW: Duration = Duration::from_secs(10);
/// Processes under this are left out: a game in a match sends well under
/// it, and so do idle apps.
pub const LISTED_KBPS: f64 = 100.0;
/// From this much a process is a problem for a game, not just a note.
pub const PROBLEM_KBPS: f64 = 1000.0;
/// At most this many processes are listed.
pub const MAX_LISTED: usize = 5;

/// What a process is, for the fix the window suggests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A game store or launcher, usually downloading or updating.
    Launcher,
    /// Cloud storage syncing.
    Sync,
    /// Windows Update and other Windows services.
    Update,
    /// A web browser, usually streaming video.
    Browser,
    /// Live streaming or recording software uploading.
    Streaming,
    Other,
}

/// Image names by kind, lowercase.
const KINDS: &[(Kind, &[&str])] = &[
    (
        Kind::Launcher,
        &[
            "steam.exe",
            "steamservice.exe",
            "epicgameslauncher.exe",
            "battle.net.exe",
            "eadesktop.exe",
            "origin.exe",
            "upc.exe",
            "ubisoftconnect.exe",
            "galaxyclient.exe",
            "riotclientservices.exe",
            "gamingservices.exe",
        ],
    ),
    (
        Kind::Sync,
        &[
            "onedrive.exe",
            "googledrivefs.exe",
            "dropbox.exe",
            "iclouddrive.exe",
            "megasync.exe",
        ],
    ),
    (
        Kind::Update,
        &[
            "svchost.exe",
            "tiworker.exe",
            "mousocoreworker.exe",
            "usocoreworker.exe",
            "wuauclt.exe",
        ],
    ),
    (
        Kind::Browser,
        &[
            "chrome.exe",
            "msedge.exe",
            "firefox.exe",
            "opera.exe",
            "brave.exe",
            "vivaldi.exe",
        ],
    ),
    (
        Kind::Streaming,
        &[
            "obs64.exe",
            "obs32.exe",
            "streamlabs obs.exe",
            "xsplit.core.exe",
        ],
    ),
];

pub fn kind(image: &str) -> Kind {
    let image = image.to_ascii_lowercase();
    KINDS
        .iter()
        .find(|(_, names)| names.contains(&image.as_str()))
        .map_or(Kind::Other, |(kind, _)| *kind)
}

/// Bytes one process moved in the window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub sent: u64,
    pub received: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Process {
    /// The executable's file name.
    pub name: String,
    pub kind: Kind,
    pub down_kbps: f64,
    pub up_kbps: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Figures {
    pub window_secs: f64,
    /// Over [`LISTED_KBPS`], busiest first, at most [`MAX_LISTED`].
    pub processes: Vec<Process>,
    /// Every process's traffic together (the service's own left out).
    pub total_down_kbps: f64,
    pub total_up_kbps: f64,
}

fn kbps(bytes: u64, window: Duration) -> f64 {
    let kbps = bytes as f64 * 8.0 / window.as_secs_f64().max(0.001) / 1000.0;
    (kbps * 10.0).round() / 10.0
}

/// Adds up usage by image name (a browser runs as many processes) and
/// keeps the busy ones. `name` gives a process ID's image name.
pub fn figures(
    usage: &HashMap<u32, Usage>,
    window: Duration,
    name: impl Fn(u32) -> String,
) -> Figures {
    let mut by_name: HashMap<String, Usage> = HashMap::new();
    let mut total = Usage::default();
    for (pid, used) in usage {
        let entry = by_name.entry(name(*pid)).or_default();
        entry.sent += used.sent;
        entry.received += used.received;
        total.sent += used.sent;
        total.received += used.received;
    }
    let mut processes: Vec<Process> = by_name
        .into_iter()
        .map(|(name, used)| Process {
            kind: kind(&name),
            name,
            down_kbps: kbps(used.received, window),
            up_kbps: kbps(used.sent, window),
        })
        .filter(|p| p.down_kbps.max(p.up_kbps) >= LISTED_KBPS)
        .collect();
    processes.sort_by(|a, b| {
        (b.down_kbps + b.up_kbps)
            .total_cmp(&(a.down_kbps + a.up_kbps))
            .then_with(|| a.name.cmp(&b.name))
    });
    processes.truncate(MAX_LISTED);
    Figures {
        window_secs: window.as_secs_f64(),
        processes,
        total_down_kbps: kbps(total.received, window),
        total_up_kbps: kbps(total.sent, window),
    }
}

pub fn findings(f: &Figures) -> Vec<Finding> {
    if f.processes.is_empty() {
        return vec![Finding::new(FindingCode::BackgroundQuiet, Severity::Info)];
    }
    f.processes
        .iter()
        .map(|p| {
            let code = match p.kind {
                Kind::Launcher => FindingCode::BackgroundLauncher,
                Kind::Sync => FindingCode::BackgroundSync,
                Kind::Update => FindingCode::BackgroundUpdate,
                Kind::Browser => FindingCode::BackgroundBrowser,
                Kind::Streaming => FindingCode::BackgroundStreaming,
                Kind::Other => FindingCode::BackgroundOther,
            };
            let severity = if p.down_kbps.max(p.up_kbps) >= PROBLEM_KBPS {
                Severity::Problem
            } else {
                Severity::Warning
            };
            Finding::new(code, severity)
                .with("name", p.name.clone())
                .with("down_kbps", p.down_kbps.round())
                .with("up_kbps", p.up_kbps.round())
        })
        .collect()
}

pub fn check(read: io::Result<Figures>) -> CheckResult {
    match read {
        Ok(figures) => {
            let found = findings(&figures);
            CheckResult::from_findings(CheckId::Background, &figures, found)
        }
        Err(e) => CheckResult::from_error(CheckId::Background, &e),
    }
}

/// Bytes each process ID sent and received over `window` (blocking).
#[cfg(windows)]
pub fn sample(window: Duration) -> io::Result<HashMap<u32, Usage>> {
    super::windows::etw::sample(window)
}

/// Whether traffic between two addresses crosses the internet line: not
/// on loopback (a local proxy hands a browser what it downloaded once),
/// and not between two addresses of the home network (a backup to a NAS,
/// a stream to a TV), which never reach the line.
pub fn on_the_line(a: IpAddr, b: IpAddr) -> bool {
    let home = |ip: IpAddr| match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_unspecified(),
        IpAddr::V6(v6) => v6.is_unicast_link_local() || v6.is_unique_local() || v6.is_unspecified(),
    };
    let (a, b) = (a.to_canonical(), b.to_canonical());
    !(a.is_loopback() || b.is_loopback() || (home(a) && home(b)))
}

/// Stops the trace session a service that ended mid-sample left running;
/// whether there was one.
#[cfg(windows)]
pub fn stop_left_behind() -> bool {
    super::windows::etw::stop_left_behind()
}

/// Watches the network for `window` (blocking) and adds up each
/// process's traffic, the calling process's own left out.
pub fn read(window: Duration) -> io::Result<Figures> {
    #[cfg(windows)]
    {
        let mut usage = sample(window)?;
        usage.remove(&std::process::id());
        Ok(figures(&usage, window, super::windows::process_name))
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "per-process network use is read on Windows only",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Status;

    #[test]
    fn only_traffic_that_crosses_the_line_counts() {
        let on = |a: &str, b: &str| on_the_line(a.parse().unwrap(), b.parse().unwrap());
        // To and from the internet.
        assert!(on("192.168.1.20", "142.250.1.1"));
        assert!(on("2001:db8::20", "2606:4700::1111"));
        assert!(on("100.64.3.4", "8.8.8.8"));
        assert!(on("0.0.0.0", "1.1.1.1"));
        // Loopback, whatever the other end says.
        assert!(!on("127.0.0.1", "127.0.0.1"));
        assert!(!on("::1", "::1"));
        assert!(!on("::ffff:127.0.0.1", "192.168.1.20"));
        // Within the home network.
        assert!(!on("192.168.1.20", "192.168.1.5"));
        assert!(!on("10.0.0.2", "172.16.0.9"));
        assert!(!on("fe80::1", "fe80::2"));
        assert!(!on("fd00::5", "fd00::6"));
        assert!(!on("169.254.1.1", "192.168.1.20"));
    }

    fn usage(entries: &[(u32, u64, u64)]) -> HashMap<u32, Usage> {
        entries
            .iter()
            .map(|&(pid, sent, received)| (pid, Usage { sent, received }))
            .collect()
    }

    fn names(pid: u32) -> String {
        match pid {
            1 | 2 => "chrome.exe",
            3 => "steam.exe",
            4 => "Discord.exe",
            5 => "OneDrive.exe",
            _ => "unknown.exe",
        }
        .into()
    }

    #[test]
    fn adds_up_by_name_and_lists_the_busy_ones() {
        let window = Duration::from_secs(10);
        // 10 s: 125,000 bytes is 100 kbit/s.
        let used = usage(&[
            (1, 10_000, 3_000_000),
            (2, 5_000, 2_000_000),
            (3, 50_000, 25_000_000),
            (4, 60_000, 60_000),
            (5, 130_000, 1_000),
        ]);
        let f = figures(&used, window, names);
        let listed: Vec<(&str, Kind)> = f
            .processes
            .iter()
            .map(|p| (p.name.as_str(), p.kind))
            .collect();
        assert_eq!(
            listed,
            [
                ("steam.exe", Kind::Launcher),
                ("chrome.exe", Kind::Browser),
                ("OneDrive.exe", Kind::Sync)
            ]
        );
        assert_eq!(f.processes[0].down_kbps, 20_000.0);
        assert_eq!(f.processes[1].down_kbps, 4_000.0);
        assert_eq!(f.processes[2].up_kbps, 104.0);
        assert_eq!(f.total_down_kbps, 24_048.8);

        let found = findings(&f);
        assert_eq!(found[0].code, FindingCode::BackgroundLauncher);
        assert_eq!(found[0].severity, Severity::Problem);
        assert_eq!(found[0].params["down_kbps"], 20_000.0);
        assert_eq!(found[2].code, FindingCode::BackgroundSync);
        assert_eq!(found[2].severity, Severity::Warning);
        assert_eq!(check(Ok(f)).status, Status::Problem);
    }

    #[test]
    fn a_quiet_network_is_good() {
        let f = figures(&usage(&[(4, 10_000, 20_000)]), WINDOW, names);
        assert!(f.processes.is_empty());
        assert_eq!(findings(&f)[0].code, FindingCode::BackgroundQuiet);
        assert_eq!(check(Ok(f)).status, Status::Good);
    }

    #[test]
    fn kinds_ignore_case() {
        assert_eq!(kind("MsEdge.exe"), Kind::Browser);
        assert_eq!(kind("svchost.exe"), Kind::Update);
        assert_eq!(kind("obs64.exe"), Kind::Streaming);
        assert_eq!(kind("game.exe"), Kind::Other);
    }

    #[cfg(not(windows))]
    #[test]
    fn elsewhere_the_check_is_unavailable() {
        let result = check(read(Duration::from_millis(1)));
        assert_eq!(result.status, Status::Unavailable);
        assert_eq!(result.findings[0].code, FindingCode::NotSupported);
    }
}
