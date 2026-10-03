//! ICMP echo with a chosen TTL, without administrator rights: the Windows
//! ICMP helper API, or Linux "ping sockets". IPv4 only for now.

use std::io;
use std::net::Ipv4Addr;
use std::time::Duration;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

#[cfg(not(any(target_os = "linux", windows)))]
mod unsupported;
#[cfg(not(any(target_os = "linux", windows)))]
use unsupported as platform;

/// Bytes after the ICMP header in every probe, like Windows `ping`'s 32.
pub const PAYLOAD: &[u8; 32] = b"PingPath probe 0123456789abcdefg";

/// What came back for one echo request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// The destination answered.
    Echo {
        from: Ipv4Addr,
        rtt: Duration,
    },
    /// A router on the way dropped the probe because its TTL ran out.
    TtlExceeded {
        from: Ipv4Addr,
        rtt: Duration,
    },
    /// A router or the destination said it can't be reached.
    Unreachable {
        from: Option<Ipv4Addr>,
        code: u8,
    },
    Timeout,
}

/// Sends ICMP echo requests from one thread. Create one per thread.
pub struct Pinger(platform::Pinger);

impl Pinger {
    /// Fails with a readable message if this system doesn't allow
    /// unprivileged ICMP.
    pub fn new() -> io::Result<Self> {
        platform::Pinger::new().map(Self)
    }

    /// Sends one echo request and waits up to `timeout` for the answer.
    pub fn send(
        &mut self,
        dst: Ipv4Addr,
        ttl: u8,
        seq: u16,
        timeout: Duration,
    ) -> io::Result<Reply> {
        self.0.send(dst, ttl, seq, timeout)
    }
}

/// Whether TTL-limited probes report the router that dropped them, which
/// traceroute needs.
pub const TRACE_SUPPORTED: bool = cfg!(any(target_os = "linux", windows));
