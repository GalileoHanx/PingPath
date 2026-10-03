//! Counters for `/metrics`, in the Prometheus text format.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Every refusal code, so each series exists from the start.
pub const REFUSAL_CODES: &[&str] = &[
    "invalid_request",
    "bad_client_key",
    "unknown_game",
    "unknown_region",
    "game_not_routable",
    "unknown_exit",
    "exit_wrong_region",
    "no_exit",
    "client_key_in_use",
    "rate_limited",
    "network_limit",
    "at_capacity",
    "unknown_session",
    "renewal_too_soon",
    "session_ended",
    "unauthorized",
    "wrong_relay",
    "bad_report",
    "internal",
];

#[derive(Debug, Default)]
pub struct Metrics {
    issued: AtomicU64,
    renewed: AtomicU64,
    latency_reports: AtomicU64,
    refusals: Mutex<BTreeMap<&'static str, u64>>,
}

impl Metrics {
    pub fn issued(&self) {
        self.issued.fetch_add(1, Ordering::Relaxed);
    }

    pub fn renewed(&self) {
        self.renewed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn latency_report(&self) {
        self.latency_reports.fetch_add(1, Ordering::Relaxed);
    }

    pub fn refused(&self, code: &'static str) {
        *self
            .refusals
            .lock()
            .expect("metrics lock")
            .entry(code)
            .or_default() += 1;
    }

    pub fn refusals(&self, code: &str) -> u64 {
        self.refusals
            .lock()
            .expect("metrics lock")
            .get(code)
            .copied()
            .unwrap_or(0)
    }

    pub fn render(&self, active_sessions: usize, uptime_secs: u64) -> String {
        let mut out = String::new();
        let mut counter = |name: &str, help: &str, value: u64| {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}"
            );
        };
        counter(
            "pingpath_control_sessions_issued_total",
            "Sessions granted.",
            self.issued.load(Ordering::Relaxed),
        );
        counter(
            "pingpath_control_sessions_renewed_total",
            "Session tokens renewed.",
            self.renewed.load(Ordering::Relaxed),
        );
        counter(
            "pingpath_control_latency_reports_total",
            "Relay latency reports accepted.",
            self.latency_reports.load(Ordering::Relaxed),
        );
        let refusals = self.refusals.lock().expect("metrics lock");
        let _ = writeln!(
            out,
            "# HELP pingpath_control_refusals_total Requests refused, by reason."
        );
        let _ = writeln!(out, "# TYPE pingpath_control_refusals_total counter");
        for code in REFUSAL_CODES
            .iter()
            .chain(refusals.keys().filter(|k| !REFUSAL_CODES.contains(k)))
        {
            let value = refusals.get(code).copied().unwrap_or(0);
            let _ = writeln!(
                out,
                "pingpath_control_refusals_total{{reason=\"{code}\"}} {value}"
            );
        }
        let _ = writeln!(
            out,
            "# HELP pingpath_control_active_sessions Sessions whose token is still valid.\n\
             # TYPE pingpath_control_active_sessions gauge\npingpath_control_active_sessions {active_sessions}"
        );
        let _ = writeln!(
            out,
            "# HELP pingpath_control_uptime_seconds Seconds since start.\n\
             # TYPE pingpath_control_uptime_seconds gauge\npingpath_control_uptime_seconds {uptime_secs}"
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_every_series() {
        let metrics = Metrics::default();
        metrics.issued();
        metrics.refused("rate_limited");
        let text = metrics.render(3, 7);
        assert!(text.contains("pingpath_control_sessions_issued_total 1\n"));
        assert!(text.contains("pingpath_control_refusals_total{reason=\"rate_limited\"} 1\n"));
        assert!(text.contains("pingpath_control_refusals_total{reason=\"no_exit\"} 0\n"));
        assert!(text.contains("pingpath_control_active_sessions 3\n"));
    }
}
