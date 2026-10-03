//! PingPath control plane and measurement API. Relay registry and session
//! tokens arrive with PLAN.md Phase 2, crowd data with Phase 6.

fn main() {
    let _logs = pingpath_diagnostics::init("control", env!("CARGO_PKG_VERSION"));
    tracing::info!("skeleton build: the control plane arrives in PLAN.md Phases 2 and 6");
}
