//! The Windows ICMP helper API (`IcmpSendEcho2`), which needs no
//! administrator rights and is what `ping` and `tracert` use.

#![allow(unsafe_code)]

use std::io;
use std::mem::size_of_val;
use std::net::Ipv4Addr;
use std::ptr;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    ICMP_ECHO_REPLY, IP_DEST_HOST_UNREACHABLE, IP_DEST_NET_UNREACHABLE, IP_DEST_PORT_UNREACHABLE,
    IP_DEST_PROT_UNREACHABLE, IP_OPTION_INFORMATION, IP_REQ_TIMED_OUT, IP_SUCCESS,
    IP_TTL_EXPIRED_REASSEM, IP_TTL_EXPIRED_TRANSIT, IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho2,
};

use super::{PAYLOAD, Reply};

pub struct Pinger {
    handle: HANDLE,
}

impl Pinger {
    pub fn new() -> io::Result<Self> {
        // SAFETY: IcmpCreateFile has no preconditions; it returns
        // INVALID_HANDLE_VALUE on failure, which is checked below.
        let handle = unsafe { IcmpCreateFile() };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle })
    }

    /// `seq` is unused: the API matches replies to requests itself.
    pub fn send(
        &mut self,
        dst: Ipv4Addr,
        ttl: u8,
        _seq: u16,
        timeout: Duration,
    ) -> io::Result<Reply> {
        let options = IP_OPTION_INFORMATION {
            Ttl: ttl,
            Tos: 0,
            Flags: 0,
            OptionsSize: 0,
            OptionsData: ptr::null_mut(),
        };
        // Room for one reply plus the echoed payload and an ICMP error's
        // quoted header, as the API asks; u64 for ICMP_ECHO_REPLY's alignment.
        let mut buffer = [0u64; 128];
        let timeout_ms = u32::try_from(timeout.as_millis())
            .unwrap_or(u32::MAX)
            .max(1);

        let start = Instant::now();
        // SAFETY: `self.handle` is a live ICMP handle owned by `self`. With no
        // event and no APC routine the call is synchronous, so the payload,
        // the options and the buffer only need to outlive this call, which
        // they do. The buffer is writable for the size passed.
        let count = unsafe {
            IcmpSendEcho2(
                self.handle,
                ptr::null_mut(),
                None,
                ptr::null(),
                u32::from_ne_bytes(dst.octets()),
                PAYLOAD.as_ptr().cast(),
                PAYLOAD.len() as u16,
                &options,
                buffer.as_mut_ptr().cast(),
                size_of_val(&buffer) as u32,
                timeout_ms,
            )
        };
        let rtt = start.elapsed();
        // SAFETY: GetLastError has no preconditions.
        let error = if count == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        // SAFETY: the buffer is zero-initialised, aligned for and larger than
        // ICMP_ECHO_REPLY, which the call writes at its start when it has a
        // reply. All-zero bytes are a valid ICMP_ECHO_REPLY.
        let reply: ICMP_ECHO_REPLY = unsafe { ptr::read(buffer.as_ptr().cast()) };

        // Addresses are in network byte order in memory.
        let from = (reply.Address != 0).then(|| Ipv4Addr::from(reply.Address.to_ne_bytes()));
        let status = if count > 0 || from.is_some() {
            reply.Status
        } else {
            error
        };
        Ok(match (status, from) {
            (IP_SUCCESS, Some(from)) if count > 0 => Reply::Echo { from, rtt },
            (IP_TTL_EXPIRED_TRANSIT | IP_TTL_EXPIRED_REASSEM, Some(from)) => {
                Reply::TtlExceeded { from, rtt }
            }
            (IP_DEST_NET_UNREACHABLE, from) => Reply::Unreachable { from, code: 0 },
            (IP_DEST_HOST_UNREACHABLE, from) => Reply::Unreachable { from, code: 1 },
            (IP_DEST_PROT_UNREACHABLE, from) => Reply::Unreachable { from, code: 2 },
            (IP_DEST_PORT_UNREACHABLE, from) => Reply::Unreachable { from, code: 3 },
            (IP_REQ_TIMED_OUT | IP_TTL_EXPIRED_TRANSIT | IP_TTL_EXPIRED_REASSEM, _) => {
                Reply::Timeout
            }
            (status, _) => {
                return Err(io::Error::other(format!(
                    "ICMP request failed with status {status}"
                )));
            }
        })
    }
}

impl Drop for Pinger {
    fn drop(&mut self) {
        // SAFETY: the handle came from IcmpCreateFile and is closed only here.
        unsafe { IcmpCloseHandle(self.handle) };
    }
}
