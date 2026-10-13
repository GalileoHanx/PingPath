//! The kernel driver service Wintun's INF registers when it installs the
//! driver, which `WintunDeleteDriver` leaves behind.
//!
//! `WintunDeleteDriver` uninstalls the driver package from the driver
//! store and nothing more, so the `wintun` service stays registered,
//! pointing at a file that is gone. PLAN.md Phase 3: uninstall leaves no
//! services behind. Installing the driver again, which Wintun does when an
//! adapter is next made (ours or another product's), registers it again.

use std::ffi::OsStr;
use std::io;
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows_service::Error;
use windows_service::service::{ServiceAccess, ServiceState, ServiceType};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_sys::Win32::Foundation::{
    ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_MARKED_FOR_DELETE,
};

/// The service name in Wintun's INF.
const SERVICE: &str = "wintun";
/// Wintun's driver file, which the service's image path must name.
const IMAGE: &str = "wintun.sys";
/// How long the driver may take to unload once the last adapter is gone.
const UNLOAD_WAIT: Duration = Duration::from_secs(5);

fn win32(e: &Error) -> Option<u32> {
    match e {
        Error::Winapi(io) => io.raw_os_error().map(|c| c as u32),
        _ => None,
    }
}

fn to_io(e: Error) -> io::Error {
    match e {
        Error::Winapi(io) => io,
        other => io::Error::other(other),
    }
}

/// Deletes the `wintun` service once the driver package is gone. `true`
/// if no such service is left; `false` if it stays because the driver is
/// still loaded or the service isn't Wintun's driver.
pub fn delete_service() -> io::Result<bool> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(to_io)?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG | ServiceAccess::DELETE;
    let service = match manager.open_service(SERVICE, access) {
        Ok(service) => service,
        Err(e) if win32(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => return Ok(true),
        Err(e) => return Err(to_io(e)),
    };
    let config = service.query_config().map_err(to_io)?;
    let wintuns = config.service_type.contains(ServiceType::KERNEL_DRIVER)
        && config
            .executable_path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.eq_ignore_ascii_case(IMAGE));
    if !wintuns {
        tracing::warn!(
            path = %config.executable_path.display(),
            "a service named wintun isn't Wintun's driver; leaving it"
        );
        return Ok(false);
    }
    let deadline = Instant::now() + UNLOAD_WAIT;
    let state = loop {
        let state = service.query_status().map_err(to_io)?.current_state;
        if state == ServiceState::Stopped || Instant::now() >= deadline {
            break state;
        }
        sleep(Duration::from_millis(100));
    };
    // Deleting a loaded driver's service would only mark it for deletion
    // until a reboot, and Wintun couldn't register it again before then.
    if state != ServiceState::Stopped {
        tracing::info!(
            ?state,
            "the Wintun driver is still loaded; leaving its service"
        );
        return Ok(false);
    }
    match service.delete() {
        Ok(()) => {}
        Err(e) if win32(&e) == Some(ERROR_SERVICE_MARKED_FOR_DELETE) => {}
        Err(e) => return Err(to_io(e)),
    }
    // The service goes when its last handle closes: this one, dropped here.
    tracing::info!("the wintun driver service is deleted");
    Ok(true)
}
