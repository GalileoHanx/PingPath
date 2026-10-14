//! Wi-Fi details from the WLAN API: signal, generation, channel and band.
//!
//! On Windows 11 24H2 and later a desktop app needs location permission
//! for the connection's details; the service, which runs this, doesn't.
//! Each query that is refused anyway is left out, not an error.

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::ptr;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::NetworkManagement::WiFi::{
    DOT11_PHY_TYPE, WLAN_BSS_ENTRY, WLAN_BSS_LIST, WLAN_CONNECTION_ATTRIBUTES, WLAN_INTF_OPCODE,
    WlanCloseHandle, WlanFreeMemory, WlanGetNetworkBssList, WlanOpenHandle, WlanQueryInterface,
    dot11_BSS_type_any, dot11_phy_type_eht, dot11_phy_type_he, dot11_phy_type_ht,
    dot11_phy_type_vht, wlan_intf_opcode_channel_number, wlan_intf_opcode_current_connection,
    wlan_intf_opcode_rssi,
};
use windows_sys::core::GUID;

use super::win32;
use crate::doctor::connection::{Band, WifiFigures};

/// The WLAN API version for Windows Vista and later.
const CLIENT_VERSION: u32 = 2;

pub fn details(interface: &GUID) -> io::Result<WifiFigures> {
    let client = Client::open()?;
    let mut figures = WifiFigures::default();
    let mut bssid = None;
    match client.query::<WLAN_CONNECTION_ATTRIBUTES>(interface, wlan_intf_opcode_current_connection)
    {
        Ok(connection) => {
            let link = connection.wlanAssociationAttributes;
            figures.quality_pct = Some(link.wlanSignalQuality);
            figures.generation = generation(link.dot11PhyType);
            // Kbit/s.
            figures.rx_mbps = Some(f64::from(link.ulRxRate) / 1000.0);
            figures.tx_mbps = Some(f64::from(link.ulTxRate) / 1000.0);
            bssid = Some(link.dot11Bssid);
        }
        Err(e) => tracing::debug!(error = %e, "no Wi-Fi connection details"),
    }
    figures.rssi_dbm = client
        .query::<i32>(interface, wlan_intf_opcode_rssi)
        .map_err(|e| tracing::debug!(error = %e, "no Wi-Fi signal strength"))
        .ok();
    figures.channel = client
        .query::<u32>(interface, wlan_intf_opcode_channel_number)
        .map_err(|e| tracing::debug!(error = %e, "no Wi-Fi channel"))
        .ok();
    let from_frequency = bssid
        .and_then(|bssid| {
            client
                .frequency_mhz(interface, bssid)
                .map_err(|e| tracing::debug!(error = %e, "no access point list"))
                .ok()
                .flatten()
        })
        .and_then(Band::from_mhz);
    figures.band = from_frequency.or_else(|| {
        figures
            .channel
            .and_then(|channel| Band::from_channel(channel, figures.generation))
    });
    Ok(figures)
}

/// Wi-Fi 4 to 7 from the PHY type; none for 802.11a/b/g.
fn generation(phy: DOT11_PHY_TYPE) -> Option<u8> {
    [
        (dot11_phy_type_ht, 4),
        (dot11_phy_type_vht, 5),
        (dot11_phy_type_he, 6),
        (dot11_phy_type_eht, 7),
    ]
    .into_iter()
    .find_map(|(kind, generation)| (kind == phy).then_some(generation))
}

struct Client(HANDLE);

impl Client {
    fn open() -> io::Result<Self> {
        let mut negotiated = 0u32;
        let mut handle: HANDLE = ptr::null_mut();
        // SAFETY: out-pointers to locals.
        win32(unsafe {
            WlanOpenHandle(CLIENT_VERSION, ptr::null(), &mut negotiated, &mut handle)
        })?;
        Ok(Self(handle))
    }

    /// One `WlanQueryInterface` value of type `T`.
    fn query<T: Copy>(&self, interface: &GUID, opcode: WLAN_INTF_OPCODE) -> io::Result<T> {
        let mut size = 0u32;
        let mut data: *mut c_void = ptr::null_mut();
        let mut kind = 0;
        // SAFETY: an open handle and out-pointers to locals; the API
        // allocates `data`, which `Memory` frees.
        win32(unsafe {
            WlanQueryInterface(
                self.0,
                interface,
                opcode,
                ptr::null(),
                &mut size,
                &mut data,
                &mut kind,
            )
        })?;
        let memory = Memory(data);
        if memory.0.is_null() || (size as usize) < size_of::<T>() {
            return Err(io::Error::other(
                "the WLAN API answered with too little data",
            ));
        }
        // SAFETY: at least size_of::<T>() bytes, and T is plain data.
        Ok(unsafe { ptr::read_unaligned(memory.0.cast::<T>()) })
    }

    /// The centre frequency of the access point `bssid`, from the list of
    /// those the adapter last saw.
    fn frequency_mhz(&self, interface: &GUID, bssid: [u8; 6]) -> io::Result<Option<u32>> {
        let mut list: *mut WLAN_BSS_LIST = ptr::null_mut();
        // SAFETY: an open handle; no SSID filter (so any BSS type), and an
        // out-pointer the API allocates and `Memory` frees.
        win32(unsafe {
            WlanGetNetworkBssList(
                self.0,
                interface,
                ptr::null(),
                dot11_BSS_type_any,
                0,
                ptr::null(),
                &mut list,
            )
        })?;
        let memory = Memory(list.cast());
        if list.is_null() {
            return Ok(None);
        }
        // SAFETY: the list holds dwNumberOfItems entries from wlanBssEntries.
        let entries = unsafe {
            let count = (*list).dwNumberOfItems as usize;
            std::slice::from_raw_parts(
                (&raw const (*list).wlanBssEntries).cast::<WLAN_BSS_ENTRY>(),
                count,
            )
        };
        let found = entries
            .iter()
            .find(|entry| entry.dot11Bssid == bssid)
            // kHz.
            .map(|entry| entry.ulChCenterFrequency / 1000);
        drop(memory);
        Ok(found)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // SAFETY: the handle WlanOpenHandle returned, closed once.
        unsafe { WlanCloseHandle(self.0, ptr::null()) };
    }
}

/// Memory the WLAN API allocated.
struct Memory(*mut c_void);

impl Drop for Memory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by the WLAN API, freed once.
            unsafe { WlanFreeMemory(self.0) };
        }
    }
}
