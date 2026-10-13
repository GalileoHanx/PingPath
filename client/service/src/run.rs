//! Starting and stopping the service: settings, games, the engine, then
//! the UI listener; on stop, Optimize off first so the routes go before
//! the process does.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use pingpath_client_core::config::ClientConfig;
use pingpath_client_core::engine::{self, EngineParts};
use pingpath_client_core::measure::{DirectProbe, ProbePlan};
use pingpath_client_core::net::Platform;
use pingpath_diagnostics::LogLevel;
use tokio::sync::{mpsc, watch};

use crate::catalog::Catalog;
use crate::paths::Paths;
use crate::server::{Caller, Limits, Service, ServiceParts};

/// How long stopping waits for the engine to end its session.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(15);

/// What the operating system tells the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemEvent {
    /// The PC is going to sleep: fall back to direct now, rather than
    /// leave routes to relays the PC won't reach when it wakes.
    Suspend,
    /// A user signed out.
    SignedOut(Caller),
}

/// Everything the service runs with.
pub struct Options {
    pub paths: Paths,
    pub platform: Arc<dyn Platform>,
    pub direct: Arc<dyn DirectProbe>,
    pub probes: ProbePlan,
    pub log_level: LogLevel,
    pub limits: Limits,
}

/// Loads the settings and games and starts the engine and the service.
/// Call on a tokio runtime.
pub fn start(options: Options) -> Arc<Service> {
    let Options {
        paths,
        platform,
        direct,
        probes,
        log_level,
        limits,
    } = options;
    let config = match ClientConfig::load(&paths.config()) {
        Ok(config) => config,
        Err(error) => {
            // Starting with the defaults keeps the UI working, so it can say
            // the service isn't set up, instead of the service restarting
            // in a loop.
            tracing::error!(%error, "cannot use the settings file; starting with the defaults");
            ClientConfig::default()
        }
    };
    if config.verbose_logging {
        log_level.set_verbose(true);
    }
    let extra_dir = config
        .games_dir
        .as_deref()
        .and_then(|dir| match paths.games_dir(dir) {
            Ok(dir) => Some(dir),
            Err(error) => {
                tracing::error!(%error, "extra game definitions not loaded");
                None
            }
        });
    let catalog = Catalog::load(extra_dir.as_deref()).unwrap_or_else(|error| {
        tracing::error!(%error, "cannot load the game definitions");
        Catalog::default()
    });
    tracing::info!(
        games = catalog.games.len(),
        extra = catalog.extra.len(),
        control_configured = !config.control_url.is_empty(),
        "service starting"
    );
    // The settings in effect, for the diagnostic report: client.toml
    // holds no secrets, and changes over the pipe are logged as they come.
    tracing::info!(settings = ?config, "settings");
    let games = catalog.infos();
    let engine = engine::start(EngineParts {
        config: config.clone(),
        games: catalog.games,
        platform,
        direct,
        probes,
    });
    Service::new(ServiceParts {
        engine,
        games,
        settings: config,
        config_path: paths.config(),
        log_level,
        log_dir: paths.logs(),
        limits,
    })
}

/// Runs `listen` (the UI listener) and handles system events until `stop`
/// says so, then turns Optimize off and stops the engine.
pub async fn serve<L, F>(
    service: Arc<Service>,
    listen: L,
    mut stop: watch::Receiver<bool>,
    mut system: mpsc::Receiver<SystemEvent>,
) -> io::Result<()>
where
    L: FnOnce(Arc<Service>, watch::Receiver<bool>) -> F,
    F: Future<Output = io::Result<()>>,
{
    let events = {
        let service = service.clone();
        tokio::spawn(async move {
            while let Some(event) = system.recv().await {
                match event {
                    SystemEvent::Suspend => {
                        tracing::info!("the PC is going to sleep");
                        service.engine().suspend().await;
                    }
                    SystemEvent::SignedOut(caller) => service.signed_out(caller).await,
                }
            }
        })
    };
    let listener = listen(service.clone(), stop.clone());
    let result = tokio::select! {
        result = listener => {
            if let Err(error) = &result {
                tracing::error!(%error, "the UI listener stopped");
            }
            result
        }
        _ = stop.wait_for(|stop| *stop) => Ok(()),
    };
    events.abort();
    tracing::info!("service stopping");
    // A hung engine must not keep the service from stopping (the
    // uninstaller waits 20 s): the adapter, and its game routes, go with
    // the process.
    if tokio::time::timeout(SHUTDOWN_WAIT, service.engine().shutdown())
        .await
        .is_err()
    {
        tracing::error!(
            after_secs = SHUTDOWN_WAIT.as_secs(),
            "the engine didn't stop in time; stopping without it"
        );
    }
    tracing::info!("service stopped");
    result
}
