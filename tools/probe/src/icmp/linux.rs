//! Linux "ping sockets" (`SOCK_DGRAM` + `IPPROTO_ICMP`): unprivileged ICMP
//! when `net.ipv4.ping_group_range` includes the user's group, which most
//! desktop distributions allow. The kernel fills in the identifier and
//! checksum and routes replies to the right socket. With `IP_RECVERR`, a
//! router's "time exceeded" arrives on the socket's error queue, which is
//! what traceroute needs.

use std::io::{self, IoSliceMut};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::socket::{
    AddressFamily, ControlMessageOwned, MsgFlags, SockFlag, SockProtocol, SockType, SockaddrIn,
    recvfrom, recvmsg, sendto, setsockopt, socket, sockopt,
};

use super::{PAYLOAD, Reply};

const ECHO_REQUEST: u8 = 8;
const ECHO_REPLY: u8 = 0;
const DEST_UNREACHABLE: u8 = 3;
const TIME_EXCEEDED: u8 = 11;
/// `SO_EE_ORIGIN_ICMP`: the queued error came from an ICMP message.
const ORIGIN_ICMP: u8 = 2;

pub struct Pinger {
    fd: OwnedFd,
    ttl: Option<u8>,
}

impl Pinger {
    pub fn new() -> io::Result<Self> {
        let fd = socket(
            AddressFamily::Inet,
            SockType::Datagram,
            SockFlag::SOCK_CLOEXEC,
            SockProtocol::Icmp,
        )
        .map_err(|e| match e {
            Errno::EACCES | Errno::EPERM => io::Error::new(
                io::ErrorKind::PermissionDenied,
                "this system does not allow unprivileged ICMP; allow it with: \
                 sudo sysctl -w net.ipv4.ping_group_range=\"0 2147483647\"",
            ),
            other => io::Error::from(other),
        })?;
        setsockopt(&fd, sockopt::Ipv4RecvErr, &true)?;
        Ok(Self { fd, ttl: None })
    }

    pub fn send(
        &mut self,
        dst: Ipv4Addr,
        ttl: u8,
        seq: u16,
        timeout: Duration,
    ) -> io::Result<Reply> {
        if self.ttl != Some(ttl) {
            setsockopt(&self.fd, sockopt::Ipv4Ttl, &i32::from(ttl))?;
            self.ttl = Some(ttl);
        }

        let start = Instant::now();
        let to = SockaddrIn::from(SocketAddrV4::new(dst, 0));
        sendto(
            self.fd.as_raw_fd(),
            &echo_request(seq),
            &to,
            MsgFlags::empty(),
        )?;

        loop {
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Ok(Reply::Timeout);
            }
            let wait = u16::try_from(remaining.as_millis().max(1)).unwrap_or(u16::MAX);
            let mut fds = [PollFd::new(self.fd.as_fd(), PollFlags::POLLIN)];
            if poll(&mut fds, PollTimeout::from(wait))? == 0 {
                return Ok(Reply::Timeout);
            }
            let events = fds[0].revents().unwrap_or(PollFlags::empty());
            // Replies and errors for earlier probes are skipped by sequence number.
            let reply = if events.contains(PollFlags::POLLERR) {
                self.read_error(seq, start)?
            } else if events.contains(PollFlags::POLLIN) {
                self.read_reply(seq, start)?
            } else {
                None
            };
            if let Some(reply) = reply {
                return Ok(reply);
            }
        }
    }

    fn read_reply(&self, seq: u16, start: Instant) -> io::Result<Option<Reply>> {
        let mut buf = [0u8; 1500];
        let (n, from) = recvfrom::<SockaddrIn>(self.fd.as_raw_fd(), &mut buf)?;
        let rtt = start.elapsed();
        let packet = &buf[..n];
        if packet.len() < 8 || packet[0] != ECHO_REPLY || sequence(packet) != seq {
            return Ok(None);
        }
        let from = from.map_or(Ipv4Addr::UNSPECIFIED, |a| a.ip());
        Ok(Some(Reply::Echo { from, rtt }))
    }

    fn read_error(&self, seq: u16, start: Instant) -> io::Result<Option<Reply>> {
        let mut buf = [0u8; 1500];
        let mut cmsg = nix::cmsg_space!(nix::libc::sock_extended_err, nix::libc::sockaddr_in);
        let mut iov = [IoSliceMut::new(&mut buf)];
        let msg = recvmsg::<SockaddrIn>(
            self.fd.as_raw_fd(),
            &mut iov,
            Some(&mut cmsg),
            MsgFlags::MSG_ERRQUEUE,
        )?;
        let rtt = start.elapsed();

        let mut reply = None;
        for message in msg.cmsgs()? {
            let ControlMessageOwned::Ipv4RecvErr(err, offender) = message else {
                continue;
            };
            if err.ee_origin != ORIGIN_ICMP {
                continue;
            }
            let from = offender.map(|a| Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr)));
            reply = match (err.ee_type, from) {
                (TIME_EXCEEDED, Some(from)) => Some(Reply::TtlExceeded { from, rtt }),
                (DEST_UNREACHABLE, from) => Some(Reply::Unreachable {
                    from,
                    code: err.ee_code,
                }),
                _ => None,
            };
        }
        let n = msg.bytes;

        // The error queue hands back the probe that bounced; make sure it's ours.
        let bounced = &buf[..n];
        if bounced.len() < 8 || sequence(bounced) != seq {
            return Ok(None);
        }
        Ok(reply)
    }
}

fn echo_request(seq: u16) -> Vec<u8> {
    let mut packet = vec![ECHO_REQUEST, 0, 0, 0, 0, 0];
    packet.extend_from_slice(&seq.to_be_bytes());
    packet.extend_from_slice(PAYLOAD);
    packet
}

fn sequence(icmp: &[u8]) -> u16 {
    u16::from_be_bytes([icmp[6], icmp[7]])
}
