//! Socket options for the tunnel's own UDP sockets.

#![allow(unsafe_code)]

use std::io;
use std::os::windows::io::AsRawSocket;

use windows_sys::Win32::Networking::WinSock::{
    IP_UNICAST_IF, IPPROTO_IP, IPPROTO_IPV6, IPV6_UNICAST_IF, SIO_UDP_CONNRESET, SOCKET,
    SOCKET_ERROR, WSAGetLastError, WSAIoctl, setsockopt,
};

fn last_socket_error() -> io::Error {
    // SAFETY: WSAGetLastError has no preconditions.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

/// Sends from `socket` only through interface `index`, whatever the route
/// table says (`IP_UNICAST_IF`). It's the backstop to the relay host
/// routes: tunnel traffic can't loop into the PingPath adapter even while
/// a host route is missing. For IPv4 the option takes the index in
/// network byte order, for IPv6 in host byte order; the wrong order fails
/// silently, pinning the socket to some other interface.
pub fn pin_to_interface(socket: &impl AsRawSocket, index: u32, ipv6: bool) -> io::Result<()> {
    let (level, option, value) = if ipv6 {
        (IPPROTO_IPV6, IPV6_UNICAST_IF, index)
    } else {
        (IPPROTO_IP, IP_UNICAST_IF, index.to_be())
    };
    // SAFETY: the socket handle is open for the duration of the call (the
    // caller borrows the socket), and the option value is 4 readable bytes.
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as SOCKET,
            level,
            option,
            (&value as *const u32).cast(),
            size_of::<u32>() as i32,
        )
    };
    if result == SOCKET_ERROR {
        return Err(last_socket_error());
    }
    Ok(())
}

/// Stops ICMP "port unreachable" replies from failing the socket's next
/// receive with `WSAECONNRESET` (`SIO_UDP_CONNRESET` off). Without this a
/// relay that restarts, or one stray reply, would surface as a receive
/// error on the socket that carries the whole session.
pub fn ignore_connection_resets(socket: &impl AsRawSocket) -> io::Result<()> {
    let off: u32 = 0;
    let mut returned = 0u32;
    // SAFETY: the socket handle is open for the duration of the call; the
    // input is a 4-byte BOOL, there is no output buffer, `returned` is a
    // valid out-pointer, and without an OVERLAPPED the call is synchronous.
    let result = unsafe {
        WSAIoctl(
            socket.as_raw_socket() as SOCKET,
            SIO_UDP_CONNRESET,
            (&off as *const u32).cast(),
            size_of::<u32>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
            None,
        )
    };
    if result == SOCKET_ERROR {
        return Err(last_socket_error());
    }
    Ok(())
}
