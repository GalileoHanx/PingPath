//! Loading wintun.dll, and reading the wintun crate's LUID type.

#![allow(unsafe_code)]

use std::io;
use std::path::Path;

use windows_sys::Win32::Foundation::{
    ERROR_BAD_EXE_FORMAT, ERROR_FILE_NOT_FOUND, ERROR_MOD_NOT_FOUND, ERROR_PATH_NOT_FOUND,
    ERROR_PROC_NOT_FOUND,
};
use windows_sys::Win32::System::LibraryLoader::{
    LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories,
};

/// Loads wintun.dll from `path`, which must be absolute.
///
/// The service runs as LocalSystem, so a DLL found through the standard
/// search order (the current folder, PATH) would run with full rights.
/// Before loading, the process's DLL search is narrowed to System32 and
/// the application's folder, which is where wintun.dll's own imports and
/// any later load must come from; wintun.dll itself is loaded by its full
/// path from the admin-only install folder.
pub fn load(path: &Path) -> io::Result<wintun::Wintun> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "wintun.dll must be given by its absolute path, not {}",
                path.display()
            ),
        ));
    }
    if !path.is_file() {
        return Err(missing(path));
    }
    // SAFETY: SetDefaultDllDirectories takes a flag set and nothing else;
    // these two flags are a valid combination.
    let narrowed = unsafe {
        SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_APPLICATION_DIR)
    };
    if narrowed == 0 {
        return Err(io::Error::other(format!(
            "cannot restrict where DLLs are loaded from: {}",
            io::Error::last_os_error()
        )));
    }
    // SAFETY: loading a DLL runs its DllMain. This is the WireGuard-signed
    // wintun.dll the installer put in the admin-only install folder, loaded
    // by absolute path; the crate then resolves the Wintun 0.14 exports
    // with the signatures from wintun.h and fails if one is missing.
    unsafe { wintun::load_from_path(path) }.map_err(|e| classify(path, e))
}

/// Removes the Wintun driver from the machine, which Wintun does only when
/// no Wintun adapter of anyone's is left (another product's VPN keeps it).
/// `Ok(false)` when it stayed for that reason or another.
pub fn remove_driver(wintun: &wintun::Wintun) -> io::Result<bool> {
    // SAFETY: WintunDeleteDriver takes no arguments and only acts on the
    // driver's own service and package; the crate resolved it with its
    // signature from wintun.h when the DLL was loaded.
    if unsafe { wintun.WintunDeleteDriver() } != 0 {
        Ok(true)
    } else {
        let error = io::Error::last_os_error();
        tracing::info!(%error, "the Wintun driver stays (another Wintun adapter may still use it)");
        Ok(false)
    }
}

fn missing(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "wintun.dll is missing at {}; reinstall PingPath",
            path.display()
        ),
    )
}

/// The Windows error behind a failed load, for a message that says what
/// to do about it.
fn classify(path: &Path, error: wintun::Error) -> io::Error {
    let code = match &error {
        wintun::Error::LibLoading(e) => std::error::Error::source(e)
            .and_then(|source| source.downcast_ref::<io::Error>())
            .and_then(io::Error::raw_os_error),
        _ => None,
    };
    match code.map(|c| c as u32) {
        Some(ERROR_MOD_NOT_FOUND | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => missing(path),
        Some(ERROR_BAD_EXE_FORMAT) => io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "wintun.dll at {} is built for another processor; reinstall PingPath",
                path.display()
            ),
        ),
        Some(ERROR_PROC_NOT_FOUND) => io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is not Wintun 0.14 (a function is missing): {error}",
                path.display()
            ),
        ),
        _ => io::Error::other(format!(
            "cannot load wintun.dll from {}: {error}",
            path.display()
        )),
    }
}

/// The adapter's LUID as a plain number. The crate's `NET_LUID_LH` comes
/// from an older windows-sys than ours, so it is passed across by value.
pub fn luid_of(adapter: &wintun::Adapter) -> u64 {
    let luid = adapter.get_luid();
    // SAFETY: both views of the union are 64 plain bits; any value is a
    // valid u64.
    unsafe { luid.Value }
}
