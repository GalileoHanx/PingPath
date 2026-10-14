//! The home network doctor (docs/decisions/011-home-network-doctor.md):
//! checks of the player's own connection, each with a status, its figures
//! and findings. A finding is a [`FindingCode`] with parameters; the window
//! turns it into a sentence and a fix in the player's language, so nothing
//! here is text a player reads.
//!
//! Each check reads the system in one place (the `read` functions, per
//! platform) and decides in another (`findings`, plain functions of the
//! figures), so the decisions are tested on every OS.

use std::io;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub mod connection;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

/// The checks, in the order a run makes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckId {
    Connection,
    Background,
    Dns,
    Bufferbloat,
}

/// How much a finding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Says what is fine, or what the figures are.
    Info,
    /// Worth fixing; the game works, but less well than it could.
    Warning,
    /// Makes games lag or disconnect.
    Problem,
}

/// A check's overall result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Good,
    Warning,
    Problem,
    /// This computer can't run the check, or not now; a finding says why.
    Unavailable,
    /// The check went wrong; `error` says how.
    Failed,
}

/// What a check can find. Each has a sentence and a fix in both of the
/// window's languages (`doctor.finding.<code>`, checked by
/// tools/check-i18n.py).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingCode {
    /// Connected by cable.
    Ethernet,
    /// Connected over Wi-Fi.
    Wifi,
    /// Wi-Fi signal below the thresholds in [`connection`]. `rssi_dbm`.
    WeakSignal,
    /// Wi-Fi on the 2.4 GHz band.
    CrowdedBand,
    /// A cable connection negotiated below 100 Mbit/s. `mbps`.
    SlowLink,
    /// Traffic to the internet leaves through a virtual adapter: another
    /// VPN or a virtual switch. `adapter`.
    VirtualAdapter,
    /// The check doesn't run on this operating system.
    NotSupported,
    /// The check went wrong; the result's `error` says how.
    CheckFailed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub code: FindingCode,
    pub severity: Severity,
    /// Numbers and names the sentence shows; never sentences.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub params: Map<String, Value>,
}

impl Finding {
    pub fn new(code: FindingCode, severity: Severity) -> Self {
        Self {
            code,
            severity,
            params: Map::new(),
        }
    }

    pub fn with(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.params.insert(name.to_string(), value.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    pub check: CheckId,
    pub status: Status,
    /// The check's own measurements (its `Figures` type), or null.
    pub figures: Value,
    pub findings: Vec<Finding>,
    /// What went wrong, for the log and the report (`Failed` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CheckResult {
    /// A result whose status is its worst finding's severity.
    pub fn from_findings(check: CheckId, figures: &impl Serialize, findings: Vec<Finding>) -> Self {
        let status = match findings.iter().map(|f| f.severity).max() {
            Some(Severity::Problem) => Status::Problem,
            Some(Severity::Warning) => Status::Warning,
            Some(Severity::Info) | None => Status::Good,
        };
        Self {
            check,
            status,
            figures: serde_json::to_value(figures).unwrap_or(Value::Null),
            findings,
            error: None,
        }
    }

    pub fn unavailable(check: CheckId, why: Finding) -> Self {
        Self {
            check,
            status: Status::Unavailable,
            figures: Value::Null,
            findings: vec![why],
            error: None,
        }
    }

    /// The check couldn't run here (`io::ErrorKind::Unsupported`), or went
    /// wrong.
    pub fn from_error(check: CheckId, error: &io::Error) -> Self {
        if error.kind() == io::ErrorKind::Unsupported {
            return Self::unavailable(
                check,
                Finding::new(FindingCode::NotSupported, Severity::Info),
            );
        }
        Self {
            check,
            status: Status::Failed,
            figures: Value::Null,
            findings: vec![Finding::new(FindingCode::CheckFailed, Severity::Info)],
            error: Some(error.to_string()),
        }
    }

    /// Logs the result with its figures and findings (PLAN.md section 9:
    /// the diagnostic report carries the last run).
    pub fn log(&self) {
        let findings: Vec<String> = self
            .findings
            .iter()
            .map(|f| {
                let code = serde_json::to_value(f.code)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                if f.params.is_empty() {
                    code
                } else {
                    format!("{code} {}", Value::Object(f.params.clone()))
                }
            })
            .collect();
        match self.status {
            Status::Failed => tracing::warn!(
                check = ?self.check,
                error = self.error.as_deref().unwrap_or(""),
                "network check failed"
            ),
            _ => tracing::info!(
                check = ?self.check,
                status = ?self.status,
                figures = %self.figures,
                findings = ?findings,
                "network check"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_is_the_worst_finding() {
        let info = Finding::new(FindingCode::Ethernet, Severity::Info);
        let warning = Finding::new(FindingCode::Wifi, Severity::Warning);
        let problem =
            Finding::new(FindingCode::WeakSignal, Severity::Problem).with("rssi_dbm", -80);
        let status = |findings: Vec<Finding>| {
            CheckResult::from_findings(CheckId::Connection, &(), findings).status
        };
        assert_eq!(status(vec![]), Status::Good);
        assert_eq!(status(vec![info.clone()]), Status::Good);
        assert_eq!(status(vec![info.clone(), warning.clone()]), Status::Warning);
        assert_eq!(status(vec![problem, warning, info]), Status::Problem);
    }

    #[test]
    fn errors_say_whether_the_check_can_run_here() {
        let unsupported = io::Error::new(io::ErrorKind::Unsupported, "not here");
        let result = CheckResult::from_error(CheckId::Background, &unsupported);
        assert_eq!(result.status, Status::Unavailable);
        assert_eq!(result.findings[0].code, FindingCode::NotSupported);
        assert_eq!(result.error, None);

        let broken = io::Error::other("the trace wouldn't start");
        let result = CheckResult::from_error(CheckId::Background, &broken);
        assert_eq!(result.status, Status::Failed);
        assert_eq!(result.findings[0].code, FindingCode::CheckFailed);
        assert_eq!(result.error.as_deref(), Some("the trace wouldn't start"));
    }

    #[test]
    fn a_result_goes_over_the_pipe_as_codes_and_numbers() {
        let result = CheckResult::from_findings(
            CheckId::Connection,
            &serde_json::json!({"medium": "wifi"}),
            vec![Finding::new(FindingCode::WeakSignal, Severity::Warning).with("rssi_dbm", -70)],
        );
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::json!({
                "check": "connection",
                "status": "warning",
                "figures": {"medium": "wifi"},
                "findings": [{"code": "weak_signal", "severity": "warning", "params": {"rssi_dbm": -70}}],
            })
        );
        let back: CheckResult =
            serde_json::from_value(serde_json::to_value(&result).unwrap()).unwrap();
        assert_eq!(back, result);
    }
}
