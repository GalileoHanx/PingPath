//! The doctor's system readings on Windows: the adapter that carries
//! traffic to the internet from IP Helper, and its Wi-Fi details from the
//! WLAN API (docs/decisions/011-home-network-doctor.md).

#![allow(unsafe_code)]

use std::io;
use std::mem::size_of;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_BUFFER_OVERFLOW, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_FRIENDLY_NAME, GAA_FLAG_SKIP_MULTICAST,
    GAA_FLAG_SKIP_UNICAST, GetAdaptersAddresses, GetBestInterfaceEx, GetIfEntry2,
    IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211, IP_ADAPTER_ADDRESSES_LH, MIB_IF_ROW2,
};
use windows_sys::Win32::NetworkManagement::Ndis::NdisPhysicalMediumNative802_11;
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, AF_UNSPEC, IN_ADDR, IN_ADDR_0, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6,
    SOCKET_ADDRESS,
};

use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

use super::connection::{Figures, Medium};

pub(crate) mod etw;
mod wlan;

/// Any public address: `GetBestInterfaceEx` only looks up the route to it.
const PUBLIC: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);
/// `MIB_IF_ROW2.InterfaceAndOperStatusFlags`: a physical connector, which
/// network cards have (Wi-Fi ones too) and VPN adapters and virtual
/// switches don't.
const CONNECTOR_PRESENT: u8 = 1 << 2;

fn win32(code: u32) -> io::Result<()> {
    if code == NO_ERROR {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

/// A NUL-terminated UTF-16 buffer as a string.
fn wide(buffer: &[u16]) -> String {
    let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

pub fn connection() -> io::Result<Figures> {
    let index = best_interface()?;
    let mut row = MIB_IF_ROW2 {
        InterfaceIndex: index,
        ..Default::default()
    };
    // SAFETY: the row is zeroed but for the index GetIfEntry2 looks up.
    win32(unsafe { GetIfEntry2(&mut row) })?;

    let connector = row.InterfaceAndOperStatusFlags._bitfield & CONNECTOR_PRESENT != 0;
    let wifi =
        row.Type == IF_TYPE_IEEE80211 || row.PhysicalMediumType == NdisPhysicalMediumNative802_11;
    let medium = if wifi {
        Medium::Wifi
    } else if connector && row.Type == IF_TYPE_ETHERNET_CSMACD {
        Medium::Ethernet
    } else {
        Medium::Other
    };
    // Bits per second; all ones when the adapter doesn't say.
    let link_mbps = (row.ReceiveLinkSpeed != 0 && row.ReceiveLinkSpeed != u64::MAX)
        .then(|| row.ReceiveLinkSpeed as f64 / 1e6);
    let details = if wifi {
        match wlan::details(&row.InterfaceGuid) {
            Ok(details) => Some(details),
            Err(e) => {
                tracing::warn!(error = %e, "the Wi-Fi details couldn't be read");
                None
            }
        }
    } else {
        None
    };
    Ok(Figures {
        medium,
        adapter: Some(wide(&row.Description)),
        is_virtual: !connector,
        link_mbps,
        wifi: details,
    })
}

/// The DNS resolvers of the adapter that carries traffic to the internet,
/// in the order Windows asks them, on port 53; a link-local one keeps its
/// scope. Windows lists fec0:0:0:ffff::1 to 3 when no IPv6 resolver is
/// set; those are left out.
pub fn resolvers() -> io::Result<Vec<SocketAddr>> {
    let index = best_interface()?;
    let flags = GAA_FLAG_SKIP_UNICAST
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_FRIENDLY_NAME;
    // The size the documentation suggests; grown if Windows asks for more.
    let mut size: u32 = 15_000;
    let mut buffer: Vec<u64> = Vec::new();
    for _ in 0..3 {
        buffer.resize((size as usize).div_ceil(8), 0);
        // SAFETY: an 8-byte-aligned buffer of at least `size` bytes.
        let code = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_UNSPEC),
                flags,
                ptr::null(),
                buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
                &mut size,
            )
        };
        if code == ERROR_BUFFER_OVERFLOW {
            continue;
        }
        win32(code)?;
        let mut found = Vec::new();
        let mut adapter = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: a list GetAdaptersAddresses built in `buffer`, which
        // outlives this walk; each pointer is null or into it.
        unsafe {
            while !adapter.is_null() {
                if (*adapter).Anonymous1.Anonymous.IfIndex == index {
                    let mut server = (*adapter).FirstDnsServerAddress;
                    while !server.is_null() {
                        if let Some(address) = socket_address(&(*server).Address) {
                            if !is_site_local_default(address.ip()) && !found.contains(&address) {
                                found.push(address);
                            }
                        }
                        server = (*server).Next;
                    }
                }
                adapter = (*adapter).Next;
            }
        }
        return Ok(found);
    }
    Err(io::Error::other("the adapter list kept growing"))
}

/// The route lookup for a public address: the internet adapter's index.
fn best_interface() -> io::Result<u32> {
    let destination = SOCKADDR_IN {
        sin_family: AF_INET,
        sin_port: 0,
        sin_addr: IN_ADDR {
            S_un: IN_ADDR_0 {
                S_addr: u32::from_ne_bytes(PUBLIC.octets()),
            },
        },
        sin_zero: [0; 8],
    };
    let mut index = 0u32;
    // SAFETY: a valid SOCKADDR_IN, read only, and a u32 to write.
    win32(unsafe { GetBestInterfaceEx((&raw const destination).cast::<SOCKADDR>(), &mut index) })?;
    Ok(index)
}

/// The address in a SOCKET_ADDRESS, if it is IPv4 or IPv6, on port 53
/// (DNS resolvers' addresses come without a port). An IPv6 one keeps its
/// scope ID, without which a link-local address can't be reached.
///
/// # Safety
/// `address.lpSockaddr` must be null or point to `iSockaddrLength` bytes.
unsafe fn socket_address(address: &SOCKET_ADDRESS) -> Option<SocketAddr> {
    let raw = address.lpSockaddr;
    if raw.is_null() {
        return None;
    }
    let len = usize::try_from(address.iSockaddrLength).ok()?;
    // SAFETY: the caller's promise, and the length checked per family.
    unsafe {
        match (*raw).sa_family {
            AF_INET if len >= size_of::<SOCKADDR_IN>() => {
                let v4 = ptr::read_unaligned(raw.cast::<SOCKADDR_IN>());
                let ip = Ipv4Addr::from(v4.sin_addr.S_un.S_addr.to_ne_bytes());
                Some(SocketAddr::new(IpAddr::V4(ip), 53))
            }
            AF_INET6 if len >= size_of::<SOCKADDR_IN6>() => {
                let v6 = ptr::read_unaligned(raw.cast::<SOCKADDR_IN6>());
                let ip = Ipv6Addr::from(v6.sin6_addr.u.Byte);
                let scope = v6.Anonymous.sin6_scope_id;
                Some(SocketAddr::V6(SocketAddrV6::new(ip, 53, 0, scope)))
            }
            _ => None,
        }
    }
}

/// A process's executable file name, for the background check; "System"
/// for the kernel, and "process <id>" for one that is gone or won't say.
pub fn process_name(pid: u32) -> String {
    if pid == 4 {
        return "System".into();
    }
    // SAFETY: the handle is checked, used once and closed.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return format!("process {pid}");
        }
        let mut buffer = [0u16; 1024];
        let mut len = buffer.len() as u32;
        let ok =
            QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut len);
        CloseHandle(handle);
        if ok == 0 {
            return format!("process {pid}");
        }
        let path = String::from_utf16_lossy(&buffer[..len as usize]);
        path.rsplit('\\').next().unwrap_or(&path).to_string()
    }
}

fn is_site_local_default(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfec0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runner's own adapter (a virtual machine's network card).
    #[test]
    fn reads_the_adapter_that_reaches_the_internet() {
        let figures = connection().expect("the connection check reads this machine's adapter");
        println!("{figures:?}");
        assert!(
            figures
                .adapter
                .as_deref()
                .is_some_and(|name| !name.is_empty())
        );
        if figures.medium != Medium::Wifi {
            assert!(figures.wifi.is_none());
        }
    }

    #[test]
    fn names_this_process() {
        let name = process_name(std::process::id());
        assert!(name.to_ascii_lowercase().ends_with(".exe"), "{name}");
        assert_eq!(process_name(4), "System");
    }

    /// The runner's resolvers, as Windows would ask them.
    #[test]
    fn reads_the_internet_adapters_resolvers() {
        let resolvers = resolvers().expect("the DNS check reads this machine's resolvers");
        println!("{resolvers:?}");
        assert!(!resolvers.is_empty(), "the runner has a DNS resolver");
        assert!(!resolvers.iter().any(|r| is_site_local_default(r.ip())));
        for resolver in &resolvers {
            assert_eq!(resolver.port(), 53);
            if let SocketAddr::V6(v6) = resolver {
                // Link-local resolvers can only be reached through their
                // interface.
                let link_local = v6.ip().segments()[0] & 0xffc0 == 0xfe80;
                assert!(!link_local || v6.scope_id() != 0, "{resolver}");
            }
        }
    }
}
