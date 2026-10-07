//! The PingPath adapter: a Wintun adapter this process creates (never
//! opens) with a fixed GUID, its address and interface settings, and the
//! two loops that move packets between it and the engine.
//!
//! Packets from the system arrive on one blocking reader thread, which
//! copies each one, frees its slot in Wintun's ring at once, and hands the
//! copy to the engine with `try_send`: when the engine falls behind the
//! packet is dropped, as a real link would, because a late game packet is
//! worse than a lost one. Packets to the system are written by an async
//! task; writing to the ring never blocks.

use std::io;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_BUFFER_OVERFLOW, ERROR_HANDLE_EOF,
};
use wintun::{Session, Wintun};

use super::dll;
use super::iphlp::{self, AddressState};
use crate::net::{ADAPTER_NAME, Adapter, AdapterConfig, InterfaceId, PACKET_QUEUE, PacketChannels};

/// The adapter's fixed GUID, {066C8B67-BABF-4B6D-8AC5-666510895AAB}. A
/// fixed GUID keeps one Windows network profile for the adapter instead
/// of "Network 2, 3, ..." on every run, and names the adapter for crash
/// cleanup.
pub const ADAPTER_GUID: u128 = 0x066c_8b67_babf_4b6d_8ac5_6665_1089_5aab;
/// The device description reads "PingPath Tunnel".
const TUNNEL_TYPE: &str = "PingPath";
/// 4 MiB of ring per direction, as in Wintun's own example: plenty for
/// game traffic, and a power of two as Wintun requires.
const RING_CAPACITY: u32 = 0x40_0000;
/// The adapter's interface metric. High, so the system never prefers the
/// adapter for anything; game prefixes reach it by longest-prefix match.
pub const INTERFACE_METRIC: u32 = 9000;
/// How long the adapter's IP interface may take to appear after Wintun
/// has created it.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the adapter's address may take to become usable before the
/// adapter is used anyway (see [`wait_until_usable`]).
const ADDRESS_TIMEOUT: Duration = Duration::from_secs(5);

/// The LUID of the adapter this process has created, while it exists.
/// The platform reads it to set our routes aside; the adapter clears it
/// when dropped.
pub type Current = Arc<Mutex<Option<u64>>>;

pub fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct WintunAdapter {
    luid: u64,
    /// The session, shared with the writer task, which uses it only while
    /// holding the lock; `Drop` takes it out so nothing writes afterwards.
    session: Arc<Mutex<Option<Arc<Session>>>>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    /// Dropping it ends the writer task.
    writer_stop: Option<oneshot::Sender<()>>,
    adapter: Option<Arc<wintun::Adapter>>,
    current: Current,
}

impl Adapter for WintunAdapter {
    fn interface(&self) -> InterfaceId {
        InterfaceId(self.luid)
    }
}

/// Creates the adapter, gives it `config`'s address and MTU, and starts
/// its packet loops. Must run inside the tokio runtime (the writer is a
/// task) and as administrator. Returns once the address is usable, which
/// can take seconds (the first adapter also installs the driver), so the
/// engine calls it off its async threads.
///
/// A stale adapter left by a crash (the same GUID) is not opened or
/// deleted here: Wintun removes orphaned adapters, those whose creating
/// process has exited, when an adapter is created, and frees the name.
pub fn create(
    wintun: &Wintun,
    config: &AdapterConfig,
    current: &Current,
) -> io::Result<(WintunAdapter, PacketChannels)> {
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
        io::Error::other("the PingPath adapter must be created inside the tokio runtime")
    })?;
    if let Some(luid) = *lock(current) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("this process already has the PingPath adapter (interface {luid:#x})"),
        ));
    }
    if let Ok(Some(stale)) = iphlp::guid_to_luid(ADAPTER_GUID) {
        tracing::warn!(
            interface = format_args!("{stale:#x}"),
            "a PingPath adapter from an earlier run is still present; Wintun removes it as this one is created"
        );
    }

    let started = Instant::now();
    // If Wintun creates the adapter but the crate's lookup of it by name
    // then fails, the crate returns the error without closing the new
    // handle (wintun 0.5.1, adapter.rs), so that adapter, with no routes,
    // stays until the service stops. Nothing here can reach the handle.
    let adapter = wintun::Adapter::create(wintun, ADAPTER_NAME, TUNNEL_TYPE, Some(ADAPTER_GUID))
        .map_err(|e| {
            // The crate drops Wintun's error code; it is still the thread's
            // last error right after the call.
            let os = io::Error::last_os_error();
            let hint = if os.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
                " (needs administrator rights)"
            } else {
                ""
            };
            io::Error::new(
                os.kind(),
                format!("cannot create the PingPath adapter: {e}: {os}{hint}"),
            )
        })?;
    // The crate finds the LUID by the adapter's name; make sure the name
    // led to the adapter just created. Returning drops `adapter`, which
    // removes it.
    //
    // The requested GUID is the interface GUID Windows reports: wintun.h
    // documents RequestedGUID as "the GUID of the created network
    // adapter", and wintun.dll (its strings) stores it as the new device's
    // SuggestedInstanceId and reads the adapter's GUID back from the
    // device's NetCfgInstanceId, the GUID ConvertInterfaceLuidToGuid
    // returns; its own name-conflict handling compares interfaces through
    // ConvertInterfaceLuidToGuid too. The Windows tests confirm it (the
    // adapter is found by its GUID). Should Windows ever not honour the
    // request, the error says so rather than blaming the name.
    let luid = dll::luid_of(&adapter);
    let guid = iphlp::luid_to_guid(luid)?;
    if guid != ADAPTER_GUID {
        let owner = match iphlp::guid_to_luid(ADAPTER_GUID) {
            Ok(Some(other)) => format!("interface {other:#x}"),
            Ok(None) => "no interface".into(),
            Err(e) => format!("unknown ({e})"),
        };
        return Err(io::Error::other(format!(
            "the interface named {ADAPTER_NAME} ({luid:#x}) has GUID {guid:032x}, not the requested {ADAPTER_GUID:032x}, which belongs to {owner}"
        )));
    }

    // Wintun reports the link as connected only while a session is open,
    // so the session starts before the address and routes are added.
    let session = Arc::new(adapter.start_session(RING_CAPACITY).map_err(|e| {
        io::Error::other(format!(
            "cannot start the PingPath adapter's session: {e}: {}",
            io::Error::last_os_error()
        ))
    })?);

    // The interface settings first: they turn off duplicate address
    // detection, so the address is usable as soon as it is added.
    when_ready("MTU and metric", || {
        iphlp::configure_ipv4(luid, u32::from(config.mtu), INTERFACE_METRIC)
    })?;
    when_ready("address", || iphlp::add_address(luid, config.address))?;
    let address_ms = wait_until_usable(luid, config.address).as_millis() as u64;
    no_ipv6_routes(luid);

    let (to_engine, incoming) = mpsc::channel(PACKET_QUEUE);
    let (outgoing, from_engine) = mpsc::channel(PACKET_QUEUE);
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let session = session.clone();
        let stop = stop.clone();
        thread::Builder::new()
            .name("pingpath-adapter-rx".into())
            .spawn(move || read_loop(&session, &to_engine, &stop))?
    };
    let shared = Arc::new(Mutex::new(Some(session)));
    let (writer_stop, stopped) = oneshot::channel();
    runtime.spawn(write_loop(shared.clone(), from_engine, stopped));

    *lock(current) = Some(luid);
    let index = adapter.get_adapter_index().unwrap_or_default();
    let driver = wintun::get_running_driver_version(wintun)
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "unknown".into());
    tracing::info!(
        name = ADAPTER_NAME,
        interface = format_args!("{luid:#x}"),
        index,
        address = %config.address,
        mtu = config.mtu,
        metric = INTERFACE_METRIC,
        wintun_driver = %driver,
        address_ms,
        took_ms = started.elapsed().as_millis() as u64,
        "adapter created"
    );
    Ok((
        WintunAdapter {
            luid,
            session: shared,
            stop,
            reader: Some(reader),
            writer_stop: Some(writer_stop),
            adapter: Some(adapter),
            current: current.clone(),
        },
        PacketChannels { incoming, outgoing },
    ))
}

/// Runs `step` until the adapter's IP interface exists: it can appear a
/// moment after Wintun returns.
fn when_ready(what: &str, mut step: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        match step() {
            Err(e) if iphlp::is_not_found(&e) && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("cannot set the PingPath adapter's {what}: {e}"),
                ));
            }
            Ok(()) => return Ok(()),
        }
    }
}

/// Waits until sockets can send from the adapter's address: Windows says
/// "preferred" once duplicate address detection is done. Before that, a
/// game packet the engine's routes send to the adapter fails or leaves
/// from another address, so the adapter is handed over only now. With
/// detection turned off this normally passes at once; if it hasn't after
/// [`ADDRESS_TIMEOUT`], the adapter is used anyway and the log says why
/// the first packets may be lost. Returns how long it waited.
fn wait_until_usable(luid: u64, address: Ipv4Addr) -> Duration {
    let started = Instant::now();
    loop {
        let state = iphlp::address_state(luid, address);
        let waited_ms = started.elapsed().as_millis() as u64;
        match state {
            Ok(Some(AddressState::Preferred)) => return started.elapsed(),
            Ok(Some(AddressState::Duplicate)) => {
                tracing::warn!(
                    %address,
                    waited_ms,
                    "Windows reports the adapter's address as a duplicate; it may not carry traffic"
                );
                return started.elapsed();
            }
            _ if started.elapsed() >= ADDRESS_TIMEOUT => {
                let state = match state {
                    Ok(Some(state)) => format!("{state:?}"),
                    Ok(None) => "not on the adapter".into(),
                    Err(e) => format!("unknown: {e}"),
                };
                tracing::warn!(
                    %address,
                    %state,
                    waited_ms,
                    "the adapter's address is still not usable; going on, and the first game packets may be lost"
                );
                return started.elapsed();
            }
            _ => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// IPv6 is never routed to the adapter (exits don't carry it): its IPv6
/// side is told not to learn routes, and the routes Windows adds to every
/// interface (link-local and multicast) are removed, so no IPv6 traffic
/// has a way in. Failures are logged and not fatal: the game routes are
/// IPv4 only either way.
fn no_ipv6_routes(luid: u64) {
    match iphlp::quiet_ipv6(luid, INTERFACE_METRIC) {
        Ok(true) => {}
        Ok(false) => tracing::debug!("IPv6 is off on the adapter"),
        Err(e) => {
            tracing::warn!(error = %e, "could not turn off IPv6 route learning on the adapter")
        }
    }
    let rows = match iphlp::route_table(iphlp::IPV6) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "could not list IPv6 routes");
            return;
        }
    };
    let mut removed = 0;
    for row in rows.iter().filter(|row| row.interface == luid) {
        match iphlp::delete_route(row) {
            Ok(_) => removed += 1,
            Err(e) => {
                tracing::warn!(destination = %row.destination, error = %e, "could not remove an IPv6 route from the adapter")
            }
        }
    }
    if removed > 0 {
        tracing::info!(removed, "removed the adapter's IPv6 routes");
    }
}

fn read_loop(session: &Arc<Session>, to_engine: &mpsc::Sender<Vec<u8>>, stop: &AtomicBool) {
    let mut dropped: u64 = 0;
    let ended = loop {
        // Under steady traffic the crate's blocking receive keeps finding
        // packets and never looks at its shutdown event, so check here too.
        if stop.load(Ordering::Relaxed) {
            break "stopped";
        }
        match session.receive_blocking() {
            Ok(packet) => {
                let copy = packet.bytes().to_vec();
                // Frees the packet's slot in the ring now.
                drop(packet);
                match to_engine.try_send(copy) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => dropped += 1,
                    Err(TrySendError::Closed(_)) => break "the engine closed the channel",
                }
            }
            Err(wintun::Error::ShuttingDown) => break "stopped",
            Err(e) => {
                // The adapter is going away (ERROR_HANDLE_EOF) or its ring
                // is corrupt. Ending the channel tells the engine.
                tracing::error!(error = %e, "the adapter stopped delivering packets");
                break "error";
            }
        }
    };
    tracing::debug!(ended, dropped, "adapter reader ended");
}

enum WriteError {
    /// The ring is full: the packet is dropped, as Wintun advises.
    Full,
    /// The adapter is being removed.
    Gone,
    Other(String),
}

async fn write_loop(
    session: Arc<Mutex<Option<Arc<Session>>>>,
    mut from_engine: mpsc::Receiver<Vec<u8>>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut dropped: u64 = 0;
    let ended = loop {
        let packet = tokio::select! {
            _ = &mut stop => break "stopped",
            packet = from_engine.recv() => match packet {
                Some(packet) => packet,
                None => break "the engine closed the channel",
            },
        };
        let written = {
            let session = lock(&session);
            let Some(session) = session.as_ref() else {
                break "stopped";
            };
            write(session, &packet)
        };
        match written {
            Ok(()) => {}
            Err(WriteError::Full) => dropped += 1,
            Err(WriteError::Gone) => break "the adapter is gone",
            Err(WriteError::Other(e)) => {
                dropped += 1;
                tracing::debug!(error = %e, len = packet.len(), "could not write a packet to the adapter");
            }
        }
    };
    tracing::debug!(ended, dropped, "adapter writer ended");
}

fn write(session: &Arc<Session>, packet: &[u8]) -> Result<(), WriteError> {
    let len = u16::try_from(packet.len())
        .ok()
        .filter(|&len| len > 0)
        .ok_or_else(|| WriteError::Other(format!("a {}-byte packet", packet.len())))?;
    match session.allocate_send_packet(len) {
        Ok(mut out) => {
            out.bytes_mut().copy_from_slice(packet);
            // Every allocated packet must be sent: Wintun sends in
            // allocation order, so an unsent one would hold up the rest.
            session.send_packet(out);
            Ok(())
        }
        Err(wintun::Error::Io(e)) if e.raw_os_error() == Some(ERROR_BUFFER_OVERFLOW as i32) => {
            Err(WriteError::Full)
        }
        Err(wintun::Error::Io(e)) if e.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) => {
            Err(WriteError::Gone)
        }
        Err(e) => Err(WriteError::Other(e.to_string())),
    }
}

impl Drop for WintunAdapter {
    /// Ends the session and removes the adapter, which removes every route
    /// through it. In order: nothing writes any more, the reader is woken
    /// and joined, the session ends once its last handle is gone, then the
    /// adapter is closed, which removes an adapter this process created.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let session = lock(&self.session).take();
        drop(self.writer_stop.take());
        let woken = match &session {
            Some(session) => match session.shutdown() {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!(error = %e, "could not wake the adapter's reader");
                    false
                }
            },
            None => true,
        };
        if let Some(reader) = self.reader.take() {
            // A reader that wasn't woken would block this forever; it then
            // ends with the process instead.
            if woken && reader.join().is_err() {
                tracing::error!("the adapter's reader thread panicked");
            }
        }
        drop(session);
        if let Some(adapter) = self.adapter.take() {
            let holders = Arc::strong_count(&adapter);
            if holders > 1 {
                tracing::warn!(
                    holders,
                    "the adapter is still in use; it is removed when the last user lets go"
                );
            }
            drop(adapter);
        }
        let mut current = lock(&self.current);
        if *current == Some(self.luid) {
            *current = None;
        }
        tracing::info!(
            name = ADAPTER_NAME,
            interface = format_args!("{:#x}", self.luid),
            "adapter removed"
        );
    }
}

/// Removes a stale PingPath adapter left by an earlier run. Wintun only
/// removes an adapter through the handle of the process that created it,
/// and a crashed process's adapter is "orphaned": creating an adapter
/// makes Wintun remove orphans first. So this creates the adapter with
/// the same GUID and drops it at once, which removes the new one too.
pub fn replace_stale(wintun: &Wintun) -> io::Result<()> {
    let adapter = wintun::Adapter::create(wintun, ADAPTER_NAME, TUNNEL_TYPE, Some(ADAPTER_GUID))
        .map_err(|e| {
            let os = io::Error::last_os_error();
            io::Error::new(
                os.kind(),
                format!("cannot replace the stale PingPath adapter: {e}: {os}"),
            )
        })?;
    drop(adapter);
    Ok(())
}
