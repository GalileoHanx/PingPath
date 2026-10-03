//! PingPath probe CLI: measures latency, jitter, loss and routing detours to
//! a game's server regions. Implemented in PLAN.md Phase 1.

fn main() {
    let _logs = pingpath_diagnostics::init("probe", env!("CARGO_PKG_VERSION"));
    tracing::info!("skeleton build: the probe arrives in PLAN.md Phase 1");
}
