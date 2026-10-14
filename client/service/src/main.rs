//! `pingpath-service`: the PingPath service. The installer runs `install`
//! and `uninstall`; Windows starts it with `service`; developers run it in
//! a console with `run`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use pingpath_service::paths::Paths;

#[derive(Parser)]
#[command(
    name = "pingpath-service",
    version,
    about = "PingPath service: runs the engine and serves the PingPath app"
)]
struct Cli {
    /// Where settings, state and logs live, for run and cleanup (default:
    /// %ProgramData%\PingPath on Windows, /var/lib/pingpath elsewhere; the
    /// installed service always uses the default)
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run in this console until Ctrl+C (as administrator or root)
    Run,
    /// Register the auto-start service and start it (Windows, as administrator)
    Install,
    /// Stop and remove the service, its routes, adapter and data (Windows, as administrator)
    Uninstall,
    /// Stop the service if it is running (Windows, as administrator)
    Stop,
    /// Remove routes and the adapter left behind by a service that crashed
    Cleanup,
    /// Started by the Windows Service Control Manager
    #[command(hide = true)]
    Service,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // The registered service always starts without --data-dir, so install
    // and uninstall must act on the folder it will use, not another one.
    if cli.data_dir.is_some()
        && matches!(
            cli.command,
            Command::Install | Command::Uninstall | Command::Service
        )
    {
        eprintln!(
            "error: --data-dir applies to run and cleanup; the installed service always uses its system folder"
        );
        return ExitCode::FAILURE;
    }
    let paths = cli.data_dir.map(Paths::under).unwrap_or_else(Paths::system);
    // What an administrator or the installer runs: its warnings (a route
    // not removed, a driver left in place) go to stderr, which the
    // installer's details show. No log file: the data folder may be on its
    // way out. run and the service log to their files.
    let _logs = matches!(
        cli.command,
        Command::Install | Command::Uninstall | Command::Stop | Command::Cleanup
    )
    .then(|| {
        pingpath_diagnostics::init_in(
            "service-cli",
            env!("CARGO_PKG_VERSION"),
            None,
            pingpath_diagnostics::Console::Quiet,
        )
    });
    let result = match cli.command {
        Command::Run => platform::run(paths),
        Command::Install => platform::install(paths),
        Command::Uninstall => platform::uninstall(paths),
        Command::Stop => platform::stop(),
        Command::Cleanup => platform::cleanup(paths),
        Command::Service => platform::service(paths),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
use pingpath_service::windows as platform;

#[cfg(not(windows))]
mod platform {
    //! Linux (development and the network-namespace tests): no service
    //! manager integration; run it with `run` as root. The Linux backend
    //! marks its routes with a protocol number instead of a ledger.

    use std::sync::Arc;

    use pingpath_client_core::ipc::UNIX_SOCKET;
    use pingpath_client_core::measure::{PingTargets, ProbePlan};
    use pingpath_client_core::net::Platform;
    use pingpath_diagnostics::Console;
    use pingpath_service::paths::Paths;
    use pingpath_service::run::{self, Options};
    use pingpath_service::server::Limits;
    use tokio::sync::{mpsc, watch};

    pub fn run(paths: Paths) -> Result<(), String> {
        let logs = pingpath_diagnostics::init_in(
            "service",
            env!("CARGO_PKG_VERSION"),
            Some(paths.logs()),
            Console::Full,
        );
        let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        runtime.block_on(async {
            let platform = backend(&paths)?;
            let service = run::start(Options {
                paths,
                platform,
                direct: Arc::new(PingTargets::default()),
                probes: ProbePlan::default(),
                log_level: logs.level(),
                limits: Limits::default(),
                doctor: Arc::new(pingpath_client_core::doctor::run::ThisComputer),
            });
            let (stop_tx, stop) = watch::channel(false);
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                let _ = stop_tx.send(true);
            });
            let (_system, system) = mpsc::channel(8);
            run::serve(
                service,
                |service, stop| async move {
                    pingpath_service::unix::listen(UNIX_SOCKET.into(), service, stop).await
                },
                stop,
                system,
            )
            .await
            .map_err(|e| e.to_string())
        })
    }

    #[cfg(target_os = "linux")]
    fn backend(_paths: &Paths) -> Result<Arc<dyn Platform>, String> {
        Ok(Arc::new(
            pingpath_client_core::net::linux::LinuxPlatform::default(),
        ))
    }

    #[cfg(not(target_os = "linux"))]
    fn backend(_paths: &Paths) -> Result<Arc<dyn Platform>, String> {
        Err("PingPath has no network backend for this system".into())
    }

    pub fn cleanup(paths: Paths) -> Result<(), String> {
        let report = backend(&paths)?.cleanup().map_err(|e| e.to_string())?;
        println!(
            "Removed {} adapter(s) and {} route(s).",
            report.adapters, report.routes
        );
        Ok(())
    }

    fn windows_only() -> Result<(), String> {
        Err("this command is for Windows; use `run` here".into())
    }

    pub fn install(_paths: Paths) -> Result<(), String> {
        windows_only()
    }

    pub fn uninstall(_paths: Paths) -> Result<(), String> {
        windows_only()
    }

    pub fn stop() -> Result<(), String> {
        windows_only()
    }

    pub fn service(_paths: Paths) -> Result<(), String> {
        windows_only()
    }
}
