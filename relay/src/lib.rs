//! The PingPath relay (PLAN.md Phase 2): entry and exit roles, session
//! tokens, the exit allowlist, rate limits, health, metrics and
//! relay-to-relay probing. Protocol: docs/decisions/004-relay-wire-protocol.md.

pub mod config;
pub mod doctor;
mod entry;
mod exit;
mod http;
pub mod metrics;
mod probe;
pub mod ratelimit;
mod relay;

pub use relay::{Relay, RelayOptions, Shared};
