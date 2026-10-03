//! Logging for every PingPath component, and the diagnostic report testers
//! send back when something goes wrong (PLAN.md section 9).
//!
//! Each binary calls [`init`] first thing in `main` and keeps the returned
//! guard alive. Logs go to stderr in human-readable form and to a daily
//! rotating JSON-lines file, `<log dir>/<component>.<date>.log`, kept for
//! [`KEEP_DAYS`] days. `pingpath-report` turns those files into one
//! shareable Markdown report.

pub mod redact;
pub mod report;

use std::env;
use std::fs;
use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

pub use redact::{Redacted, redact_ip, redact_text};

/// Log filter, in `tracing` `EnvFilter` syntax: `debug`, `warn`,
/// `pingpath_relay=trace,info`, ...
pub const LOG_ENV: &str = "PINGPATH_LOG";
/// Overrides the log directory.
pub const LOG_DIR_ENV: &str = "PINGPATH_LOG_DIR";
pub const DEFAULT_FILTER: &str = "info";
pub const KEEP_DAYS: usize = 7;
/// Message of the first event every component logs; the report reads the
/// component's version from it.
pub const STARTED: &str = "logging started";

/// Keeps the background log writer running; drop it last, at the end of
/// `main`, so buffered lines reach the file.
#[must_use = "logs are lost if the guard is dropped early"]
pub struct LogGuard {
    _file: Option<WorkerGuard>,
}

/// Where logs are written: `PINGPATH_LOG_DIR` if set, otherwise
/// `%LOCALAPPDATA%\PingPath\logs` on Windows, `~/Library/Logs/PingPath` on
/// macOS and `$XDG_STATE_HOME/pingpath/logs` (default
/// `~/.local/state/pingpath/logs`) elsewhere.
pub fn log_dir() -> Option<PathBuf> {
    let var = |name| {
        env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if let Some(dir) = var(LOG_DIR_ENV) {
        return Some(dir);
    }
    if cfg!(windows) {
        var("LOCALAPPDATA").map(|d| d.join("PingPath").join("logs"))
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library/Logs/PingPath"))
    } else {
        var("XDG_STATE_HOME")
            .or_else(|| var("HOME").map(|h| h.join(".local/state")))
            .map(|d| d.join("pingpath/logs"))
    }
}

/// Sets up logging for `component` (e.g. `"relay"`) in [`log_dir`].
pub fn init(component: &str, version: &str) -> LogGuard {
    init_in(component, version, log_dir())
}

/// Like [`init`], with an explicit log directory; `None` logs to stderr only.
/// If the directory can't be used, logging falls back to stderr and says so.
pub fn init_in(component: &str, version: &str, dir: Option<PathBuf>) -> LogGuard {
    let filter =
        EnvFilter::try_from_env(LOG_ENV).unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let filter_text = filter.to_string();

    let appender = dir.as_ref().map(|dir| {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        RollingFileAppender::builder()
            .rotation(Rotation::DAILY)
            .filename_prefix(component)
            .filename_suffix("log")
            .max_log_files(KEEP_DAYS)
            .build(dir)
            .map_err(|e| e.to_string())
    });
    let (file_layer, guard, file_error) = match appender {
        Some(Ok(appender)) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let layer = fmt::layer().json().with_writer(writer);
            (Some(layer), Some(guard), None)
        }
        Some(Err(e)) => (None, None, Some(e)),
        None => (None, None, None),
    };

    let initialised = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(file_layer)
        .try_init()
        .is_ok();
    if !initialised {
        // Another subscriber is already installed (e.g. in tests); keep it.
        return LogGuard { _file: guard };
    }

    log_panics();
    tracing::info!(
        component,
        version,
        os = env::consts::OS,
        arch = env::consts::ARCH,
        log_filter = filter_text,
        log_dir = dir.as_ref().map(|d| d.display().to_string()),
        "{STARTED}"
    );
    if let Some(error) = file_error {
        tracing::warn!(%error, "cannot write log files; logging to the console only");
    }
    LogGuard { _file: guard }
}

/// Records panics in the log as well as on stderr, so crashes show up in
/// the diagnostic report.
fn log_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location().map(ToString::to_string);
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        tracing::error!(location, message, "panic");
        previous(info);
    }));
}
