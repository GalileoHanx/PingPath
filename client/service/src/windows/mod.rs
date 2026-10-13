//! Windows: the service registered with the Service Control Manager, the
//! named pipe the app connects to, and the permissions on
//! `%ProgramData%\PingPath`.

mod pipe;
mod scm;
pub mod security;

use std::path::PathBuf;
use std::sync::Arc;

use pingpath_client_core::measure::{PingTargets, ProbePlan};
use pingpath_client_core::net::Platform;
use pingpath_client_core::net::windows::WindowsPlatform;
use pingpath_diagnostics::Console;
use tokio::sync::{mpsc, watch};

use crate::paths::Paths;
use crate::run::{self, Options, SystemEvent};
use crate::server::Limits;
pub use scm::SERVICE_NAME;

/// `wintun.dll` sits next to the service in its install folder, which only
/// administrators can change (it is inside Program Files: see
/// [`require_program_files`]); it is loaded from there by full path.
fn wintun_dll() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(exe.with_file_name("wintun.dll"))
}

fn backend(paths: &Paths) -> Result<Arc<dyn Platform>, String> {
    let dll = wintun_dll()?;
    let platform = WindowsPlatform::new(&dll, paths.route_ledger())
        .map_err(|e| format!("cannot use {}: {e}", dll.display()))?;
    Ok(Arc::new(platform))
}

/// Runs the service until `stop`: secures the data folder, starts logging,
/// the engine and the pipe. `running` is called once it is up.
fn serve(
    paths: Paths,
    stop: watch::Receiver<bool>,
    system: mpsc::Receiver<SystemEvent>,
    running: impl FnOnce(),
) -> Result<(), String> {
    // Before logging starts, so log files inherit the logs' permissions.
    let secured = security::prepare_data_dir(&paths);
    let logs = pingpath_diagnostics::init_in(
        "service",
        env!("CARGO_PKG_VERSION"),
        Some(paths.logs()),
        Console::Full,
    );
    if let Err(error) = secured {
        tracing::error!(%error, data = %paths.data.display(), "cannot secure the data folder");
        return Err(format!("cannot secure {}: {error}", paths.data.display()));
    }
    let result = run_engine(paths, stop, system, running, logs.level());
    // Logged here, while the log file is open: `logs` flushes and closes it
    // when this returns, and a line logged after that is lost. A wintun.dll
    // that won't load ends here, and the log must say why.
    if let Err(error) = &result {
        tracing::error!(%error, "the service failed");
    }
    result
}

fn run_engine(
    paths: Paths,
    stop: watch::Receiver<bool>,
    system: mpsc::Receiver<SystemEvent>,
    running: impl FnOnce(),
    log_level: pingpath_diagnostics::LogLevel,
) -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let result = runtime.block_on(async {
        let platform = backend(&paths)?;
        let service = run::start(Options {
            paths,
            platform,
            direct: Arc::new(PingTargets::default()),
            probes: ProbePlan::default(),
            log_level,
            limits: Limits::default(),
        });
        running();
        run::serve(service, pipe::listen, stop, system)
            .await
            .map_err(|e| e.to_string())
    });
    // Let the runtime's threads finish before the service reports Stopped.
    drop(runtime);
    result
}

/// `run`: the service in this console, until Ctrl+C.
pub fn run(paths: Paths) -> Result<(), String> {
    security::require_elevation()?;
    // The engine cleans up as it starts, before the pipe would show that
    // the service already runs.
    require_service_stopped("run it here")?;
    let (stop_tx, stop) = watch::channel(false);
    let (_system, system) = mpsc::channel(8);
    std::thread::spawn(move || {
        if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            runtime.block_on(async {
                let _ = tokio::signal::ctrl_c().await;
            });
        }
        let _ = stop_tx.send(true);
    });
    serve(paths, stop, system, || {
        println!("PingPath service running; press Ctrl+C to stop.");
    })
}

/// `service`: started by Windows.
pub fn service(paths: Paths) -> Result<(), String> {
    scm::dispatch(paths)
}

/// `install`: the data folder, then the service, started.
pub fn install(paths: Paths) -> Result<(), String> {
    security::require_elevation()?;
    require_program_files()?;
    security::prepare_data_dir(&paths)
        .map_err(|e| format!("cannot prepare {}: {e}", paths.data.display()))?;
    scm::install()?;
    println!("The PingPath service is installed and running.");
    Ok(())
}

/// The service runs as LocalSystem from its install folder and loads
/// wintun.dll from it, so only administrators may be able to change that
/// folder. Program Files is such a folder; one made elsewhere (on C:\ or
/// another drive) usually lets every signed-in user change it. The
/// installer refuses any folder outside Program Files, and `install`
/// holds a copy registered by hand to the same rule.
fn require_program_files() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // ProgramW6432 is the 64-bit Program Files even for a process started
    // by the 32-bit installer, whose ProgramFiles is the (x86) one.
    let program_files = std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .map(PathBuf::from)
        .ok_or("cannot find the Program Files folder")?;
    // Canonical paths: no "..", and the case as it is on disk.
    if let (Ok(exe), Ok(root)) = (
        std::fs::canonicalize(&exe),
        std::fs::canonicalize(&program_files),
    ) {
        if exe.starts_with(&root) && exe.parent() != Some(root.as_path()) {
            return Ok(());
        }
    }
    Err(format!(
        "{} must be in a folder inside {}: the service runs with full system rights, \
         so only administrators may be able to change its files. Install PingPath with its installer.",
        exe.display(),
        program_files.display()
    ))
}

/// `stop`: for the installer, before it replaces the files.
pub fn stop() -> Result<(), String> {
    security::require_elevation()?;
    if scm::stop()? {
        println!("The PingPath service is stopped.");
    }
    Ok(())
}

/// Refuses while the service runs: its relay host routes are in the same
/// ledger and its adapter has the same GUID, so cleaning up beside it
/// would take the live session's host routes away and try to replace its
/// adapter.
fn require_service_stopped(what: &str) -> Result<(), String> {
    if scm::running()? {
        return Err(format!(
            "the PingPath service is running; stop it first (pingpath-service stop), then {what}"
        ));
    }
    Ok(())
}

/// `cleanup`: routes and the adapter left by a service that crashed. The
/// service does the same each time it starts.
pub fn cleanup(paths: Paths) -> Result<(), String> {
    security::require_elevation()?;
    require_service_stopped("clean up")?;
    let report = backend(&paths)?.cleanup().map_err(|e| e.to_string())?;
    println!(
        "Removed {} adapter(s) and {} route(s).",
        report.adapters, report.routes
    );
    Ok(())
}

/// `uninstall`: the service (which removes its routes and adapter as it
/// stops), anything a crash left behind, then the data folder.
pub fn uninstall(paths: Paths) -> Result<(), String> {
    security::require_elevation()?;
    scm::uninstall()?;
    // The route ledger lives in the data folder: keep it if cleanup failed,
    // so a later `cleanup` can still find what to remove.
    cleanup(paths.clone())?;
    // The driver Wintun installed when the adapter was first made, unless
    // another product's Wintun adapter still uses it.
    let dll = wintun_dll()?;
    match WindowsPlatform::new(&dll, paths.route_ledger()).and_then(|p| p.remove_driver()) {
        Ok(true) => println!("Removed the Wintun driver."),
        Ok(false) => println!("Left the Wintun driver: another program's Wintun adapter uses it."),
        Err(e) => eprintln!("warning: cannot remove the Wintun driver: {e}"),
    }
    match std::fs::remove_dir_all(&paths.data) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot remove {}: {e}", paths.data.display())),
    }
    println!("PingPath's service, routes, adapter and data are removed.");
    Ok(())
}
