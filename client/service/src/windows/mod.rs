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
/// administrators can change; it is loaded from there by full path.
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
    let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let result = runtime.block_on(async {
        let platform = backend(&paths)?;
        let service = run::start(Options {
            paths,
            platform,
            direct: Arc::new(PingTargets::default()),
            probes: ProbePlan::default(),
            log_level: logs.level(),
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
    security::prepare_data_dir(&paths)
        .map_err(|e| format!("cannot prepare {}: {e}", paths.data.display()))?;
    scm::install()?;
    println!("The PingPath service is installed and running.");
    Ok(())
}

/// `stop`: for the installer, before it replaces the files.
pub fn stop() -> Result<(), String> {
    security::require_elevation()?;
    if scm::stop()? {
        println!("The PingPath service is stopped.");
    }
    Ok(())
}

/// `cleanup`: routes and the adapter left by a service that crashed. The
/// service does the same each time it starts.
pub fn cleanup(paths: Paths) -> Result<(), String> {
    security::require_elevation()?;
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
    match std::fs::remove_dir_all(&paths.data) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot remove {}: {e}", paths.data.display())),
    }
    println!("PingPath's service, routes, adapter and data are removed.");
    Ok(())
}
