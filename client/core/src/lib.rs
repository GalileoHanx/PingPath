//! PingPath client engine (docs/decisions/007-client-architecture.md):
//! split-tunnel routes for the selected game only, path selection against
//! the direct route, and the fail-open watchdog with entry failover.
//!
//! The engine is platform-independent; [`net`] holds the adapter and route
//! table backends (Wintun and IP Helper on Windows, TUN and `ip` on Linux
//! for tests) and an in-memory network for engine tests. The Windows
//! service (`client/service`) runs the engine; the Tauri UI talks to it
//! over a named pipe.

pub mod config;
pub mod control;
pub mod engine;
pub mod filter;
pub mod measure;
pub mod net;
pub mod plan;
pub mod policy;
pub mod select;
pub mod stats;
