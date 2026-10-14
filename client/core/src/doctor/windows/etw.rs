//! Per-process network use from the Microsoft-Windows-Kernel-Network
//! event provider, read through a real-time trace session: the way
//! Resource Monitor shows it. Needs an administrator (the service is
//! LocalSystem).
//!
//! Every TCP and UDP send and receive event of the provider starts with
//! the process ID and the size, both 32-bit, whatever follows (IPv4 or
//! IPv6 addresses, ports, sequence numbers).

use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::ptr;
use std::sync::Mutex;
use std::time::Duration;

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_CTX_CLOSE_PENDING};
use windows_sys::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, EVENT_CONTROL_CODE_ENABLE_PROVIDER,
    EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES,
    EVENT_TRACE_REAL_TIME_MODE, EnableTraceEx2, OpenTraceW, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_REAL_TIME, PROCESSTRACE_HANDLE, ProcessTrace, StartTraceW,
    TRACE_LEVEL_INFORMATION, WNODE_FLAG_TRACED_GUID,
};
use windows_sys::core::GUID;

use super::win32;
use crate::doctor::background::Usage;

/// Microsoft-Windows-Kernel-Network.
const PROVIDER: GUID = GUID::from_u128(0x7dd42a49_5329_4832_8dfd_43d979153a88);
/// KERNEL_NETWORK_KEYWORD_IPV4 | KERNEL_NETWORK_KEYWORD_IPV6.
const KEYWORDS: u64 = 0x10 | 0x20;
/// Data sent: TCP over IPv4 and IPv6, then UDP over IPv4 and IPv6.
const SENT: [u16; 4] = [10, 26, 42, 58];
/// Data received, likewise.
const RECEIVED: [u16; 4] = [11, 27, 43, 59];
/// A fixed name, so a session a crash left behind is found and stopped.
const SESSION: &str = "PingPath network doctor";
/// What OpenTraceW returns when it fails (on 64-bit Windows; a 32-bit
/// process sees the low half).
const INVALID: [u64; 2] = [u64::MAX, u32::MAX as u64];

/// EVENT_TRACE_PROPERTIES followed by room for the session's name, which
/// the system writes there.
struct Properties {
    buffer: Vec<u64>,
}

impl Properties {
    fn new(name: &[u16]) -> Self {
        let size = size_of::<EVENT_TRACE_PROPERTIES>() + name.len() * 2;
        let mut properties = Self {
            buffer: vec![0; size.div_ceil(8)],
        };
        let p = properties.as_mut();
        p.Wnode.BufferSize = size as u32;
        p.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        // Query performance counter timestamps.
        p.Wnode.ClientContext = 1;
        p.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        // Deliver events at least every second.
        p.FlushTimer = 1;
        p.LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        properties
    }

    fn as_mut(&mut self) -> &mut EVENT_TRACE_PROPERTIES {
        // SAFETY: the buffer is 8-byte aligned and at least that large.
        unsafe { &mut *self.buffer.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>() }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Stops the session by name; errors (none running) don't matter.
fn stop(name: &[u16]) {
    let mut properties = Properties::new(name);
    // SAFETY: a valid name and properties buffer.
    unsafe {
        ControlTraceW(
            CONTROLTRACE_HANDLE { Value: 0 },
            name.as_ptr(),
            properties.as_mut(),
            EVENT_TRACE_CONTROL_STOP,
        )
    };
}

unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    // SAFETY: ProcessTrace passes a valid record for the call's duration,
    // and UserContext is the Mutex `sample` keeps alive until it returns.
    let record = unsafe { &*record };
    let provider = &record.EventHeader.ProviderId;
    let ours = provider.data1 == PROVIDER.data1
        && provider.data2 == PROVIDER.data2
        && provider.data3 == PROVIDER.data3
        && provider.data4 == PROVIDER.data4;
    if !ours || record.UserDataLength < 8 {
        return;
    }
    let id = record.EventHeader.EventDescriptor.Id;
    let sent = SENT.contains(&id);
    if !sent && !RECEIVED.contains(&id) {
        return;
    }
    // SAFETY: UserDataLength bytes at UserData, checked to be at least 8.
    let data = unsafe { std::slice::from_raw_parts(record.UserData.cast::<u8>(), 8) };
    let pid = u32::from_le_bytes(data[..4].try_into().expect("4 bytes"));
    let size = u64::from(u32::from_le_bytes(data[4..8].try_into().expect("4 bytes")));
    let usage = unsafe { &*record.UserContext.cast::<Mutex<HashMap<u32, Usage>>>() };
    let mut usage = usage.lock().unwrap_or_else(|e| e.into_inner());
    let entry = usage.entry(pid).or_default();
    if sent {
        entry.sent += size;
    } else {
        entry.received += size;
    }
}

/// Bytes sent and received per process ID over `window` (blocking).
pub fn sample(window: Duration) -> io::Result<HashMap<u32, Usage>> {
    let name = wide(SESSION);
    let mut properties = Properties::new(&name);
    let mut session = CONTROLTRACE_HANDLE { Value: 0 };
    // SAFETY: a valid name and properties buffer; the handle is written.
    let mut code = unsafe { StartTraceW(&mut session, name.as_ptr(), properties.as_mut()) };
    if code == ERROR_ALREADY_EXISTS {
        tracing::info!("stopping a network doctor trace left behind");
        stop(&name);
        properties = Properties::new(&name);
        // SAFETY: as above.
        code = unsafe { StartTraceW(&mut session, name.as_ptr(), properties.as_mut()) };
    }
    win32(code)?;
    // Whatever happens next, the session stops.
    struct Session<'a>(&'a [u16]);
    impl Drop for Session<'_> {
        fn drop(&mut self) {
            stop(self.0);
        }
    }
    let _session = Session(&name);

    // SAFETY: a running session and the provider's GUID.
    win32(unsafe {
        EnableTraceEx2(
            session,
            &PROVIDER,
            EVENT_CONTROL_CODE_ENABLE_PROVIDER,
            TRACE_LEVEL_INFORMATION as u8,
            KEYWORDS,
            0,
            0,
            ptr::null(),
        )
    })?;

    let usage: Mutex<HashMap<u32, Usage>> = Mutex::new(HashMap::new());
    let mut logger_name = name.clone();
    // SAFETY: plain data; zero is a valid starting point for every field.
    let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
    logfile.LoggerName = logger_name.as_mut_ptr();
    logfile.Anonymous1.ProcessTraceMode =
        PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
    logfile.Anonymous2.EventRecordCallback = Some(on_event);
    logfile.Context = (&raw const usage).cast_mut().cast::<c_void>();
    // SAFETY: the logfile names a running real-time session, and `usage`
    // and `logger_name` outlive the trace (closed below, in this scope).
    let trace: PROCESSTRACE_HANDLE = unsafe { OpenTraceW(&mut logfile) };
    if INVALID.contains(&trace.Value) {
        return Err(io::Error::last_os_error());
    }
    std::thread::scope(|scope| {
        let consumer = scope.spawn(|| {
            // SAFETY: an open trace handle; returns once the session stops
            // or the trace is closed.
            unsafe { ProcessTrace(&trace, 1, ptr::null(), ptr::null()) }
        });
        std::thread::sleep(window);
        stop(&name);
        // SAFETY: the handle OpenTraceW returned, closed once.
        let closed = unsafe { CloseTrace(trace) };
        if closed != 0 && closed != ERROR_CTX_CLOSE_PENDING {
            tracing::debug!(code = closed, "closing the network doctor trace");
        }
        let _ = consumer.join();
    });
    Ok(usage.into_inner().unwrap_or_else(|e| e.into_inner()))
}
