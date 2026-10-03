//! PingPath relay (entry and exit roles). Implemented in PLAN.md Phase 2.

fn main() {
    let _logs = pingpath_diagnostics::init("relay", env!("CARGO_PKG_VERSION"));
    tracing::info!("skeleton build: the relay arrives in PLAN.md Phase 2");
}
