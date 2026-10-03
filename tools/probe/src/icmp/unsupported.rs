use std::io;
use std::net::Ipv4Addr;
use std::time::Duration;

use super::Reply;

pub struct Pinger;

impl Pinger {
    pub fn new() -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "ICMP probes are supported on Windows and Linux only; use TCP targets (address:port)",
        ))
    }

    pub fn send(&mut self, _: Ipv4Addr, _: u8, _: u16, _: Duration) -> io::Result<Reply> {
        unreachable!("Pinger::new always fails on this platform")
    }
}
