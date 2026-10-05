//! The PingPath service (docs/decisions/007-client-architecture.md): runs
//! the client engine with the privileges routes need, and serves the
//! unprivileged UI over a local channel.
//!
//! - [`run`] starts everything and stops it cleanly;
//! - [`server`] answers the UI and protects users of one PC from each other;
//! - [`catalog`] loads the games, [`paths`] says where files live;
//! - `windows` holds the Windows service, the named pipe, installation and
//!   folder permissions; `unix` the socket used on Linux (development and
//!   the network-namespace tests).

pub mod catalog;
pub mod paths;
pub mod run;
pub mod server;

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;
