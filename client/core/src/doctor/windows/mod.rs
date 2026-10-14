//! The doctor's system readings on Windows: the adapter that carries
//! traffic to the internet from IP Helper, and its Wi-Fi details from the
//! WLAN API (docs/decisions/011-home-network-doctor.md).

#![allow(unsafe_code)]

use std::io;
use std::net::Ipv4Addr;

use windows_sys::Win32::Foundation::NO_ERROR;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetBestInterfaceEx, GetIfEntry2, IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211, MIB_IF_ROW2,
};
use windows_sys::Win32::NetworkManagement::Ndis::NdisPhysicalMediumNative802_11;
use windows_sys::Win32::Networking::WinSock::{AF_INET, IN_ADDR, IN_ADDR_0, SOCKADDR, SOCKADDR_IN};

use super::connection::{Figures, Medium};

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
}
