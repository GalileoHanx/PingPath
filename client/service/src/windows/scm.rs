//! The Windows service: registration with the Service Control Manager,
//! the service's main function and its control handler.
//!
//! The service starts automatically and is restarted if it fails, so the
//! UI, which never elevates, always finds it (docs/decisions/007).

use std::ffi::{OsStr, OsString};
use std::sync::{Arc, OnceLock};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use windows_service::service::{
    PowerEventParam, ServiceAccess, ServiceAction, ServiceActionType, ServiceControl,
    ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus,
    ServiceType, SessionChangeReason,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{Error, define_windows_service, service_dispatcher};
use windows_sys::Win32::Foundation::{
    ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_SERVICE_ALREADY_RUNNING,
    ERROR_SERVICE_CANNOT_ACCEPT_CTRL, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS,
    ERROR_SERVICE_MARKED_FOR_DELETE, ERROR_SERVICE_NOT_ACTIVE,
};

use crate::paths::Paths;
use crate::run::SystemEvent;
use crate::server::Caller;

pub const SERVICE_NAME: &str = "PingPath";
const DISPLAY_NAME: &str = "PingPath";
const DESCRIPTION: &str =
    "Routes the selected game's traffic through PingPath relays when it lowers your ping.";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
/// The argument the service is registered with, so `main` knows the
/// Service Control Manager started it.
const SERVICE_ARG: &str = "service";
/// How long stopping may take before the uninstaller gives up waiting.
const STOP_WAIT: Duration = Duration::from_secs(20);
/// How long the power handler waits for the session to end before the
/// PC sleeps.
const SUSPEND_WAIT: Duration = Duration::from_millis(1500);
/// How long starting may take before `install` reports a failure.
const START_WAIT: Duration = Duration::from_secs(30);

fn win32(e: &Error) -> Option<u32> {
    match e {
        Error::Winapi(io) => io.raw_os_error().map(|c| c as u32),
        _ => None,
    }
}

/// windows-service's own messages hide the Windows error; show it.
fn describe(e: &Error) -> String {
    match e {
        Error::Winapi(io) => io.to_string(),
        other => other.to_string(),
    }
}

fn status(state: ServiceState, accept: ServiceControlAccept, wait: Duration) -> ServiceStatus {
    ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: wait,
        process_id: None,
    }
}

static PATHS: OnceLock<Paths> = OnceLock::new();

/// `pingpath-service service`: hands the process to the Service Control
/// Manager; returns when the service has stopped.
pub fn dispatch(paths: Paths) -> Result<(), String> {
    let _ = PATHS.set(paths);
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| {
        if win32(&e) == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) {
            "this command is for Windows to start the service; use `run` in a console".to_string()
        } else {
            describe(&e)
        }
    })
}

define_windows_service!(ffi_service_main, service_main);

/// Runs on a thread the Service Control Manager creates; must report
/// Stopped before returning. A panic here would abort the process.
fn service_main(_arguments: Vec<OsString>) {
    let paths = PATHS.get().cloned().unwrap_or_else(Paths::system);
    let (stop_tx, stop) = watch::channel(false);
    let (system_tx, system) = mpsc::channel(16);
    let handle: Arc<OnceLock<ServiceStatusHandle>> = Arc::new(OnceLock::new());

    // Runs on the dispatcher thread; keep it short. windows-service frees
    // this closure after it returns from Stop, Shutdown or Preshutdown, so
    // it reports StopPending, accepting nothing more, before returning:
    // Windows then sends it no other control.
    let reporter = handle.clone();
    let handler = move |control| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Shutdown | ServiceControl::Preshutdown => {
                if let Some(handle) = reporter.get() {
                    let _ = handle.set_service_status(status(
                        ServiceState::StopPending,
                        ServiceControlAccept::empty(),
                        Duration::from_secs(10),
                    ));
                }
                let _ = stop_tx.send(true);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::PowerEvent(PowerEventParam::Suspend) => {
                // Windows doesn't wait for the service before sleeping, so
                // the handler does, for at most SUSPEND_WAIT (Windows allows
                // about 2 s): the session ends and its routes go before the
                // PC sleeps, rather than at wake through the watchdog.
                let (done, ended) = std::sync::mpsc::channel();
                if system_tx.try_send(SystemEvent::Suspend(done)).is_ok() {
                    let _ = ended.recv_timeout(SUSPEND_WAIT);
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::SessionChange(change)
                if change.reason == SessionChangeReason::SessionLogoff =>
            {
                let caller = Caller::WindowsSession(change.notification.session_id);
                let _ = system_tx.try_send(SystemEvent::SignedOut(caller));
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::PowerEvent(_)
            | ServiceControl::SessionChange(_)
            | ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let registered = match service_control_handler::register(SERVICE_NAME, handler) {
        Ok(registered) => registered,
        Err(e) => {
            eprintln!(
                "cannot register the service control handler: {}",
                describe(&e)
            );
            return;
        }
    };
    let _ = handle.set(registered);
    let _ = registered.set_service_status(status(
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        Duration::from_secs(10),
    ));

    let running = move || {
        let _ = registered.set_service_status(status(
            ServiceState::Running,
            ServiceControlAccept::STOP
                | ServiceControlAccept::SHUTDOWN
                | ServiceControlAccept::POWER_EVENT
                | ServiceControlAccept::SESSION_CHANGE,
            Duration::ZERO,
        ));
    };
    let result = super::serve(paths, stop, system, running);

    // A non-zero exit code makes Windows apply the restart actions even
    // though the process didn't crash.
    let exit_code = match &result {
        Ok(()) => ServiceExitCode::NO_ERROR,
        // serve() has logged the error while its log file was open.
        Err(_) => ServiceExitCode::ServiceSpecific(1),
    };
    let _ = registered.set_service_status(ServiceStatus {
        exit_code,
        ..status(
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            Duration::ZERO,
        )
    });
}

/// Registers the service (or updates an existing registration) and
/// starts it.
pub fn install() -> Result<(), String> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|e| describe(&e))?;
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: SERVICE_TYPE,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe().map_err(|e| e.to_string())?,
        launch_arguments: vec![SERVICE_ARG.into()],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::CHANGE_CONFIG;
    // An uninstall just before can leave the old registration "marked for
    // deletion" until every handle to it closes (services.msc, for one).
    let deadline = Instant::now() + Duration::from_secs(15);
    let service = loop {
        match manager.create_service(&info, access) {
            Ok(service) => break service,
            Err(e) if win32(&e) == Some(ERROR_SERVICE_EXISTS) => {
                let service = manager
                    .open_service(SERVICE_NAME, access)
                    .map_err(|e| describe(&e))?;
                service.change_config(&info).map_err(|e| describe(&e))?;
                break service;
            }
            Err(e)
                if win32(&e) == Some(ERROR_SERVICE_MARKED_FOR_DELETE)
                    && Instant::now() < deadline =>
            {
                sleep(Duration::from_millis(500));
            }
            Err(e) if win32(&e) == Some(ERROR_SERVICE_MARKED_FOR_DELETE) => {
                return Err("the previous PingPath service is still being removed; close Services (services.msc) or restart Windows, then install again".into());
            }
            Err(e) => return Err(describe(&e)),
        }
    };
    service
        .set_description(DESCRIPTION)
        .map_err(|e| describe(&e))?;
    service
        .update_failure_actions(ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(1),
                },
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(5),
                },
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(30),
                },
            ]),
        })
        .map_err(|e| describe(&e))?;
    service
        .set_failure_actions_on_non_crash_failures(true)
        .map_err(|e| describe(&e))?;
    match service.start::<&OsStr>(&[]) {
        Ok(()) => {}
        Err(e) if win32(&e) == Some(ERROR_SERVICE_ALREADY_RUNNING) => {}
        Err(e) => return Err(describe(&e)),
    }
    // StartService returns once the service's thread exists, before it has
    // loaded wintun.dll or opened its pipe. Wait until it says it runs, so
    // a service that can't start fails the installation visibly.
    let deadline = Instant::now() + START_WAIT;
    loop {
        let status = service.query_status().map_err(|e| describe(&e))?;
        match status.current_state {
            ServiceState::Running => return Ok(()),
            ServiceState::Stopped => {
                return Err(format!(
                    "the PingPath service stopped as it started ({:?}); its log in %ProgramData%\\PingPath\\logs says why",
                    status.exit_code
                ));
            }
            _ if Instant::now() >= deadline => {
                return Err("the PingPath service did not start in time".into());
            }
            _ => sleep(Duration::from_millis(250)),
        }
    }
}

/// Whether the service is registered and not stopped (running, starting
/// or stopping).
pub fn running() -> Result<bool, String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| describe(&e))?;
    let service = match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(service) => service,
        Err(e) if win32(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => return Ok(false),
        Err(e) => return Err(describe(&e)),
    };
    let state = service.query_status().map_err(|e| describe(&e))?;
    Ok(state.current_state != ServiceState::Stopped)
}

/// Stops the service if it is registered and running, and waits for it.
/// `Ok(false)` if it isn't registered.
pub fn stop() -> Result<bool, String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| describe(&e))?;
    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP,
    ) {
        Ok(service) => service,
        Err(e) if win32(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => return Ok(false),
        Err(e) => return Err(describe(&e)),
    };
    stop_and_wait(&service)?;
    Ok(true)
}

fn stop_and_wait(service: &windows_service::service::Service) -> Result<(), String> {
    let state = |service: &windows_service::service::Service| {
        service
            .query_status()
            .map(|s| s.current_state)
            .map_err(|e| describe(&e))
    };
    if state(service)? != ServiceState::Stopped {
        match service.stop() {
            Ok(_) => {}
            Err(e) if win32(&e) == Some(ERROR_SERVICE_NOT_ACTIVE) => {}
            // Starting or already stopping: wait for it below.
            Err(e) if win32(&e) == Some(ERROR_SERVICE_CANNOT_ACCEPT_CTRL) => {}
            Err(e) => return Err(describe(&e)),
        }
    }
    let deadline = Instant::now() + STOP_WAIT;
    while state(service)? != ServiceState::Stopped {
        if Instant::now() >= deadline {
            return Err("the PingPath service did not stop in time".into());
        }
        sleep(Duration::from_millis(250));
    }
    Ok(())
}

/// Stops and deletes the service, and waits until Windows has removed it.
pub fn uninstall() -> Result<(), String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| describe(&e))?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let service = match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => service,
        Err(e) if win32(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => return Ok(()),
        Err(e) => return Err(describe(&e)),
    };
    // Marked for deletion first, so that a stop that fails still removes
    // the service once it stops, or at the next restart. Windows deletes
    // it when it has stopped and no handle to it is open.
    match service.delete() {
        Ok(()) => {}
        Err(e) if win32(&e) == Some(ERROR_SERVICE_MARKED_FOR_DELETE) => {}
        Err(e) => return Err(describe(&e)),
    }
    stop_and_wait(&service)?;
    drop(service);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
            Err(e) if win32(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => return Ok(()),
            _ => sleep(Duration::from_millis(250)),
        }
    }
    eprintln!(
        "The PingPath service is stopped and will be removed once Services (services.msc) is closed or Windows restarts."
    );
    Ok(())
}
