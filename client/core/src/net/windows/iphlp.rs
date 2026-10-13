//! IP Helper calls: routes, the adapter's address and interface settings,
//! and interface lookups. Each wrapper converts between the system's rows
//! and [`RouteRow`], so the decisions about them stay in
//! [`crate::net::route_table`], where every OS tests them.
//!
//! The "2" functions return their error code instead of setting the
//! thread's last error, so the return value is what gets converted.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ptr;

use ipnet::IpNet;
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_NOT_FOUND, ERROR_OBJECT_ALREADY_EXISTS,
    NO_ERROR, WIN32_ERROR,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    ConvertInterfaceGuidToLuid, ConvertInterfaceLuidToGuid, ConvertInterfaceLuidToIndex,
    CreateIpForwardEntry2, CreateUnicastIpAddressEntry, DeleteIpForwardEntry2, FreeMibTable,
    GetBestRoute2, GetIpForwardEntry2, GetIpForwardTable2, GetIpInterfaceEntry,
    GetIpInterfaceTable, GetUnicastIpAddressEntry, GetUnicastIpAddressTable, IP_ADDRESS_PREFIX,
    InitializeIpForwardEntry, InitializeIpInterfaceEntry, InitializeUnicastIpAddressEntry,
    MIB_IPFORWARD_ROW2, MIB_IPFORWARD_TABLE2, MIB_IPINTERFACE_ROW, MIB_IPINTERFACE_TABLE,
    MIB_UNICASTIPADDRESS_ROW, MIB_UNICASTIPADDRESS_TABLE, SetIpInterfaceEntry,
};
use windows_sys::Win32::NetworkManagement::Ndis::NET_LUID_LH;
use windows_sys::Win32::Networking::WinSock::{
    ADDRESS_FAMILY, AF_INET, AF_INET6, AF_UNSPEC, IN_ADDR, IN_ADDR_0, IN6_ADDR, IN6_ADDR_0,
    IpDadStateDuplicate, IpDadStatePreferred, IpDadStateTentative, IpPrefixOriginManual,
    IpSuffixOriginManual, MIB_IPPROTO_NETMGMT, NlroManual, RouterDiscoveryDisabled, SOCKADDR_IN,
    SOCKADDR_IN6, SOCKADDR_INET,
};
use windows_sys::core::GUID;

use crate::net::route_table::{InterfaceState, RouteRow};

/// Both address families, for [`route_table`].
pub const ANY_FAMILY: ADDRESS_FAMILY = AF_UNSPEC;
pub const IPV6: ADDRESS_FAMILY = AF_INET6;

fn check(code: WIN32_ERROR) -> io::Result<()> {
    if code == NO_ERROR {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

/// Whether `error` means the route, address or interface doesn't exist:
/// Windows answers `ERROR_NOT_FOUND`, or `ERROR_FILE_NOT_FOUND` when the
/// interface itself is gone.
pub fn is_not_found(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == ERROR_NOT_FOUND as i32 || code == ERROR_FILE_NOT_FOUND as i32
    )
}

pub fn family_of(ip: IpAddr) -> ADDRESS_FAMILY {
    match ip {
        IpAddr::V4(_) => AF_INET,
        IpAddr::V6(_) => AF_INET6,
    }
}

fn sockaddr(ip: IpAddr) -> SOCKADDR_INET {
    // Writing a union field is safe; only reading one needs `unsafe`.
    let mut address = SOCKADDR_INET::default();
    match ip {
        IpAddr::V4(v4) => {
            address.Ipv4 = SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: 0,
                // S_addr is in network byte order: its bytes in memory are
                // the octets as written.
                sin_addr: IN_ADDR {
                    S_un: IN_ADDR_0 {
                        S_addr: u32::from_ne_bytes(v4.octets()),
                    },
                },
                sin_zero: [0; 8],
            };
        }
        IpAddr::V6(v6) => {
            address.Ipv6 = SOCKADDR_IN6 {
                sin6_family: AF_INET6,
                sin6_port: 0,
                sin6_flowinfo: 0,
                sin6_addr: IN6_ADDR {
                    u: IN6_ADDR_0 { Byte: v6.octets() },
                },
                Anonymous: Default::default(),
            };
        }
    }
    address
}

fn ip_of(address: &SOCKADDR_INET) -> Option<IpAddr> {
    // SAFETY: `si_family` overlays the family field of both variants, and
    // only the variant the family names is read.
    unsafe {
        match address.si_family {
            AF_INET => Some(IpAddr::V4(Ipv4Addr::from(
                address.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes(),
            ))),
            AF_INET6 => Some(IpAddr::V6(Ipv6Addr::from(address.Ipv6.sin6_addr.u.Byte))),
            _ => None,
        }
    }
}

fn luid(value: u64) -> NET_LUID_LH {
    NET_LUID_LH { Value: value }
}

fn luid_value(luid: &NET_LUID_LH) -> u64 {
    // SAFETY: both views of the union are 64 plain bits; any value is a
    // valid u64.
    unsafe { luid.Value }
}

fn to_mib(row: &RouteRow) -> MIB_IPFORWARD_ROW2 {
    let mut mib = MIB_IPFORWARD_ROW2::default();
    // SAFETY: `mib` is a valid, writable row; this sets the defaults
    // (infinite lifetimes) that a zeroed row doesn't mean.
    unsafe { InitializeIpForwardEntry(&mut mib) };
    mib.InterfaceLuid = luid(row.interface);
    mib.DestinationPrefix = IP_ADDRESS_PREFIX {
        Prefix: sockaddr(row.destination.network()),
        PrefixLength: row.destination.prefix_len(),
    };
    // The next hop must have the destination's family even when on-link.
    mib.NextHop = sockaddr(row.next_hop.unwrap_or_else(|| row.on_link_next_hop()));
    mib.Metric = row.metric;
    // What `route add` uses too, so it marks nothing: our rows are
    // recognised by interface, ledger and metric (net::route_table).
    mib.Protocol = MIB_IPPROTO_NETMGMT;
    mib.Origin = NlroManual;
    mib
}

fn from_mib(mib: &MIB_IPFORWARD_ROW2) -> Option<RouteRow> {
    let destination = ip_of(&mib.DestinationPrefix.Prefix)?;
    let destination = IpNet::new(destination, mib.DestinationPrefix.PrefixLength).ok()?;
    Some(RouteRow {
        interface: luid_value(&mib.InterfaceLuid),
        destination,
        next_hop: ip_of(&mib.NextHop).filter(|hop| !hop.is_unspecified()),
        metric: mib.Metric,
    })
}

/// Adds a route. `Ok(false)`: a route with the same interface, destination
/// and next hop already exists (its metric may differ).
pub fn create_route(row: &RouteRow) -> io::Result<bool> {
    let mib = to_mib(row);
    // SAFETY: `mib` is an initialised row that outlives the call.
    match unsafe { CreateIpForwardEntry2(&mib) } {
        NO_ERROR => Ok(true),
        ERROR_OBJECT_ALREADY_EXISTS => Ok(false),
        code => Err(io::Error::from_raw_os_error(code as i32)),
    }
}

/// Deletes the route with `row`'s interface, destination and next hop.
/// `Ok(false)`: it was already gone, or its interface was.
pub fn delete_route(row: &RouteRow) -> io::Result<bool> {
    let mib = to_mib(row);
    // SAFETY: `mib` is an initialised row that outlives the call.
    match check(unsafe { DeleteIpForwardEntry2(&mib) }) {
        Ok(()) => Ok(true),
        Err(e) if is_not_found(&e) || interface_gone(row.interface) => Ok(false),
        Err(e) => Err(e),
    }
}

/// The route with `row`'s interface, destination and next hop as the
/// system has it now (with its own metric), if there is one.
pub fn get_route(row: &RouteRow) -> io::Result<Option<RouteRow>> {
    let mut mib = to_mib(row);
    // SAFETY: `mib` is an initialised, writable row whose key fields are
    // set; the call fills in the rest.
    match check(unsafe { GetIpForwardEntry2(&mut mib) }) {
        Ok(()) => Ok(from_mib(&mib)),
        Err(e) if is_not_found(&e) || interface_gone(row.interface) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether the interface no longer exists. Its routes went with it, so a
/// route on it is gone whatever error code the route call chose for that
/// (a Wi-Fi stick unplugged, or the adapter removed before its routes).
pub fn interface_gone(interface: u64) -> bool {
    matches!(luid_to_index(interface), Err(e) if is_not_found(&e))
}

/// Frees a table from a `Get*Table2` call when dropped, so a panic while
/// copying rows can't leak it.
struct MibTable<T>(*mut T);

impl<T> Drop for MibTable<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from a Get*Table2 call that
            // succeeded, and is freed once, here.
            unsafe { FreeMibTable(self.0.cast()) };
        }
    }
}

/// Every route of `family` (`ANY_FAMILY` for both).
pub fn route_table(family: ADDRESS_FAMILY) -> io::Result<Vec<RouteRow>> {
    let mut table: *mut MIB_IPFORWARD_TABLE2 = ptr::null_mut();
    // SAFETY: `table` is a valid out-pointer; on success it owns a table
    // that `MibTable` frees.
    check(unsafe { GetIpForwardTable2(family, &mut table) })?;
    let table = MibTable(table);
    // SAFETY: on success the table holds `NumEntries` rows laid out one
    // after the other from `Table` (declared with one element, C's
    // flexible array style), and stays valid until `MibTable` frees it.
    let rows = unsafe {
        let count = (*table.0).NumEntries as usize;
        let first = ptr::addr_of!((*table.0).Table).cast::<MIB_IPFORWARD_ROW2>();
        std::slice::from_raw_parts(first, count)
    };
    Ok(rows.iter().filter_map(from_mib).collect())
}

/// Every unicast address on this PC, with its on-link prefix length.
pub fn local_networks() -> io::Result<Vec<ipnet::IpNet>> {
    let mut table: *mut MIB_UNICASTIPADDRESS_TABLE = ptr::null_mut();
    // SAFETY: as in `route_table`.
    check(unsafe { GetUnicastIpAddressTable(AF_UNSPEC, &mut table) })?;
    let table = MibTable(table);
    // SAFETY: as in `route_table`.
    let rows = unsafe {
        let count = (*table.0).NumEntries as usize;
        let first = ptr::addr_of!((*table.0).Table).cast::<MIB_UNICASTIPADDRESS_ROW>();
        std::slice::from_raw_parts(first, count)
    };
    Ok(rows
        .iter()
        .filter_map(|row| ipnet::IpNet::new(ip_of(&row.Address)?, row.OnLinkPrefixLength).ok())
        .collect())
}

/// Each interface's metric and connection state for `family`, by LUID.
pub fn interface_table(family: ADDRESS_FAMILY) -> io::Result<HashMap<u64, InterfaceState>> {
    let mut table: *mut MIB_IPINTERFACE_TABLE = ptr::null_mut();
    // SAFETY: as in `route_table`.
    check(unsafe { GetIpInterfaceTable(family, &mut table) })?;
    let table = MibTable(table);
    // SAFETY: as in `route_table`.
    let rows = unsafe {
        let count = (*table.0).NumEntries as usize;
        let first = ptr::addr_of!((*table.0).Table).cast::<MIB_IPINTERFACE_ROW>();
        std::slice::from_raw_parts(first, count)
    };
    Ok(rows
        .iter()
        .map(|row| {
            (
                luid_value(&row.InterfaceLuid),
                InterfaceState {
                    metric: row.Metric,
                    connected: row.Connected,
                },
            )
        })
        .collect())
}

/// The route the system itself would use for `dst` now. The source
/// address it would pick is the player's own and is not returned.
pub fn best_route(dst: IpAddr) -> io::Result<RouteRow> {
    let destination = sockaddr(dst);
    let mut mib = MIB_IPFORWARD_ROW2::default();
    let mut source = SOCKADDR_INET::default();
    // SAFETY: a null interface, index 0 and a null source mean "any"; the
    // destination and both out-pointers are valid for the call.
    check(unsafe {
        GetBestRoute2(
            ptr::null(),
            0,
            ptr::null(),
            &destination,
            0,
            &mut mib,
            &mut source,
        )
    })?;
    from_mib(&mib).ok_or_else(|| io::Error::other("the system's route has no usable destination"))
}

/// Gives the interface `address`/32: no subnet, so no on-link route
/// besides the address itself. The row asks for the address to be
/// preferred (usable) at once, as wireguard-windows does; Windows may
/// still check it first, which [`address_state`] tells.
pub fn add_address(interface: u64, address: Ipv4Addr) -> io::Result<()> {
    let mut row = MIB_UNICASTIPADDRESS_ROW::default();
    // SAFETY: `row` is a valid, writable row.
    unsafe { InitializeUnicastIpAddressEntry(&mut row) };
    row.InterfaceLuid = luid(interface);
    row.Address = sockaddr(IpAddr::V4(address));
    row.OnLinkPrefixLength = 32;
    row.PrefixOrigin = IpPrefixOriginManual;
    row.SuffixOrigin = IpSuffixOriginManual;
    row.ValidLifetime = u32::MAX;
    row.PreferredLifetime = u32::MAX;
    row.DadState = IpDadStatePreferred;
    // SAFETY: `row` is initialised and outlives the call.
    match unsafe { CreateUnicastIpAddressEntry(&row) } {
        NO_ERROR => Ok(()),
        // Windows also answers this when another interface has the address
        // (wireguard-windows works around it for adapters left behind), and
        // then this one has none: game traffic routed here could not be
        // sent. Only the address on this interface counts as done.
        ERROR_OBJECT_ALREADY_EXISTS => match address_state(interface, address)? {
            Some(_) => Ok(()),
            None => Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another interface already has the adapter's address {address}"),
            )),
        },
        code => Err(io::Error::from_raw_os_error(code as i32)),
    }
}

/// What duplicate address detection says about an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressState {
    /// Usable: sockets may send from it.
    Preferred,
    /// Still being checked: sockets can't use it yet.
    Tentative,
    /// Another host on the link has it; it will never be usable.
    Duplicate,
    /// Any other `NL_DAD_STATE` (invalid, deprecated), by its value.
    Other(i32),
}

/// The state of `address` on the interface, or `None` if the interface
/// doesn't have it.
pub fn address_state(interface: u64, address: Ipv4Addr) -> io::Result<Option<AddressState>> {
    // The interface and the address are the row's key.
    let mut row = MIB_UNICASTIPADDRESS_ROW {
        InterfaceLuid: luid(interface),
        Address: sockaddr(IpAddr::V4(address)),
        ..Default::default()
    };
    // SAFETY: `row` is a valid, writable row whose key fields are set; the
    // call fills in the rest.
    match check(unsafe { GetUnicastIpAddressEntry(&mut row) }) {
        Ok(()) => Ok(Some(match row.DadState {
            state if state == IpDadStatePreferred => AddressState::Preferred,
            state if state == IpDadStateTentative => AddressState::Tentative,
            state if state == IpDadStateDuplicate => AddressState::Duplicate,
            other => AddressState::Other(other),
        })),
        Err(e) if is_not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Reads the interface's settings for `family`, lets `change` edit them,
/// and writes them back. `Ok(false)`: the interface has no such family.
fn edit_interface(
    interface: u64,
    family: ADDRESS_FAMILY,
    change: impl FnOnce(&mut MIB_IPINTERFACE_ROW),
) -> io::Result<bool> {
    let mut row = MIB_IPINTERFACE_ROW::default();
    // SAFETY: `row` is a valid, writable row.
    unsafe { InitializeIpInterfaceEntry(&mut row) };
    row.Family = family;
    row.InterfaceLuid = luid(interface);
    // SAFETY: Family and InterfaceLuid are set, as the call requires.
    match check(unsafe { GetIpInterfaceEntry(&mut row) }) {
        Ok(()) => {}
        Err(e) if family == AF_INET6 && is_not_found(&e) => return Ok(false),
        Err(e) => return Err(e),
    }
    change(&mut row);
    if family == AF_INET {
        // Set refuses a non-zero site prefix length for IPv4, which Get
        // can return.
        row.SitePrefixLength = 0;
    }
    // SAFETY: `row` is a complete row from GetIpInterfaceEntry.
    check(unsafe { SetIpInterfaceEntry(&mut row) })?;
    Ok(true)
}

/// The adapter's IPv4 settings: the tunnel MTU, a fixed, high metric, and
/// no duplicate address detection. Nothing else is on this point-to-point
/// link to clash with, and without the check an address added afterwards
/// is usable at once instead of "tentative" for a while (wireguard-windows
/// sets the same on its adapters).
pub fn configure_ipv4(interface: u64, mtu: u32, metric: u32) -> io::Result<()> {
    edit_interface(interface, AF_INET, |row| {
        row.NlMtu = mtu;
        row.UseAutomaticMetric = false;
        row.Metric = metric;
        row.DadTransmits = 0;
    })
    .map(|_| ())
}

/// The adapter's IPv6 side, which PingPath never uses: no router
/// discovery, so no prefix or default route can be learned on it, default
/// routes refused, and a high metric. `Ok(false)`: IPv6 is off on it.
pub fn quiet_ipv6(interface: u64, metric: u32) -> io::Result<bool> {
    edit_interface(interface, AF_INET6, |row| {
        row.RouterDiscoveryBehavior = RouterDiscoveryDisabled;
        row.ManagedAddressConfigurationSupported = false;
        row.OtherStatefulConfigurationSupported = false;
        row.DisableDefaultRoutes = true;
        row.DadTransmits = 0;
        row.UseAutomaticMetric = false;
        row.Metric = metric;
    })
}

pub fn luid_to_index(interface: u64) -> io::Result<u32> {
    let luid = luid(interface);
    let mut index = 0u32;
    // SAFETY: both pointers are valid for the call.
    check(unsafe { ConvertInterfaceLuidToIndex(&luid, &mut index) })?;
    Ok(index)
}

/// The interface GUID of a LUID, as the u128 the wintun crate takes.
pub fn luid_to_guid(interface: u64) -> io::Result<u128> {
    let luid = luid(interface);
    let mut guid = GUID::from_u128(0);
    // SAFETY: both pointers are valid for the call.
    check(unsafe { ConvertInterfaceLuidToGuid(&luid, &mut guid) })?;
    Ok((u128::from(guid.data1) << 96)
        | (u128::from(guid.data2) << 80)
        | (u128::from(guid.data3) << 64)
        | u128::from(u64::from_be_bytes(guid.data4)))
}

/// The LUID of the interface with this GUID, if the system has one.
pub fn guid_to_luid(guid: u128) -> io::Result<Option<u64>> {
    let guid = GUID::from_u128(guid);
    let mut found = luid(0);
    // SAFETY: both pointers are valid for the call.
    match check(unsafe { ConvertInterfaceGuidToLuid(&guid, &mut found) }) {
        Ok(()) => Ok(Some(luid_value(&found))),
        // Neither pointer is null, so an invalid parameter can only mean
        // that no interface has this GUID.
        Err(e) if is_not_found(&e) || e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) => {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}
