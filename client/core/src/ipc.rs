//! The service's IPC protocol (docs/decisions/007-client-architecture.md):
//! JSON lines over the named pipe `\\.\pipe\pingpath` (a Unix socket on
//! Linux). The UI sends requests with an `id` and gets one response for
//! each; the service also pushes [`EngineEvent`]s. The first request must
//! be `hello` with the protocol version, so a mismatched UI and service
//! say so instead of misbehaving.
//!
//! ```text
//! → {"id":1,"method":"hello","params":{"version":1,"client":"PingPath 0.1.0"}}
//! ← {"id":1,"result":{"version":1,"service":"0.1.0"}}
//! → {"id":2,"method":"measure","params":{"game":"counter-strike-2","region":"eu-central"}}
//! ← {"event":"status","data":{"mode":"measuring",...}}
//! ← {"id":2,"result":{"game":"counter-strike-2",...}}
//! → {"id":3,"method":"enable","params":{"acknowledged_warning":false}}
//! ← {"id":3,"error":{"code":"warning_not_acknowledged","message":"..."}}
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::engine::{EngineError, EngineEvent};

pub const PIPE_NAME: &str = r"\\.\pipe\pingpath";
pub const UNIX_SOCKET: &str = "/run/pingpath/service.sock";
pub const PROTOCOL_VERSION: u32 = 1;
/// Longer lines are refused unread (a local user can't make the service
/// allocate without bound).
pub const MAX_LINE: usize = 64 * 1024;

/// A request from the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Hello {
        version: u32,
        client: String,
    },
    /// The games the service knows, with whether each can be optimized.
    Games,
    Status,
    Measure {
        game: String,
        region: String,
    },
    Enable {
        #[serde(default)]
        acknowledged_warning: bool,
    },
    Disable,
    /// The player dismissed a notice.
    Dismiss {
        id: u64,
    },
    Settings,
    /// Only the settings a player may change; the control plane URL and
    /// extra game definitions are set by an administrator in the file.
    SetSettings(SettingsChange),
    /// Where the service writes its logs, for the diagnostic report.
    LogPaths,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsChange {
    #[serde(default)]
    pub verbose_logging: Option<bool>,
    #[serde(default)]
    pub min_improvement_ms: Option<f64>,
    #[serde(default)]
    pub min_improvement_pct: Option<f64>,
}

/// A request line: an `id` plus the request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

/// What a failed request returns. `code` is stable, for translations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new("bad_request", message)
    }
}

impl From<&EngineError> for ErrorBody {
    fn from(e: &EngineError) -> Self {
        let code = serde_json::to_value(e)
            .ok()
            .and_then(|v| v.get("code").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| "error".to_string());
        Self {
            code,
            message: e.to_string(),
        }
    }
}

/// A line from the service: a response or a pushed event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Ok { id: u64, result: Value },
    Err { id: u64, error: ErrorBody },
    Event(Value),
}

impl Message {
    pub fn ok(id: u64, result: impl Serialize) -> Self {
        Message::Ok {
            id,
            result: serde_json::to_value(result).unwrap_or(Value::Null),
        }
    }

    pub fn err(id: u64, error: ErrorBody) -> Self {
        Message::Err { id, error }
    }

    pub fn event(event: &EngineEvent) -> Self {
        Message::Event(serde_json::to_value(event).unwrap_or(Value::Null))
    }

    /// One line of JSON, newline included.
    pub fn to_line(&self) -> String {
        let mut line = serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string());
        line.push('\n');
        line
    }
}

/// `hello`'s answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    pub version: u32,
    pub service: String,
}

/// A game as the UI lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameInfo {
    pub id: String,
    pub name: String,
    /// `untested`, `testing`, `supported` or `unsupported`.
    pub status: String,
    /// Whether Optimize can be offered at all (rule 7).
    pub routable: bool,
    /// Still being tested: the player must acknowledge a warning first.
    pub needs_warning: bool,
    pub regions: Vec<RegionInfo>,
    /// From an administrator's extra definitions folder, not built in.
    pub test_definition: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegionInfo {
    pub id: String,
    pub name: String,
}

/// The settings as the UI shows them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsView {
    /// Shown, not editable from the UI.
    pub control_url: String,
    pub verbose_logging: bool,
    pub min_improvement_ms: f64,
    pub min_improvement_pct: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogPaths {
    pub service_logs: String,
}

/// Parses one request line; the error is the response to send.
pub fn parse_request(line: &str) -> Result<Envelope, (Option<u64>, ErrorBody)> {
    if line.len() > MAX_LINE {
        return Err((None, ErrorBody::bad_request("request too long")));
    }
    serde_json::from_str::<Envelope>(line).map_err(|e| {
        let id = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|v| v.get("id").and_then(Value::as_u64));
        (id, ErrorBody::bad_request(e.to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Mode, Status};

    #[test]
    fn requests_round_trip_as_documented() {
        let line = r#"{"id":2,"method":"measure","params":{"game":"cs2","region":"eu-central"}}"#;
        let envelope = parse_request(line).unwrap();
        assert_eq!(envelope.id, 2);
        assert_eq!(
            envelope.request,
            Request::Measure {
                game: "cs2".into(),
                region: "eu-central".into()
            }
        );
        assert_eq!(serde_json::to_string(&envelope).unwrap(), line);
        let disable = parse_request(r#"{"id":3,"method":"disable"}"#).unwrap();
        assert_eq!(disable.request, Request::Disable);
        let enable = parse_request(r#"{"id":4,"method":"enable","params":{}}"#).unwrap();
        assert_eq!(
            enable.request,
            Request::Enable {
                acknowledged_warning: false
            }
        );
    }

    #[test]
    fn refuses_bad_and_oversized_requests() {
        let (id, error) = parse_request(r#"{"id":5,"method":"reboot"}"#).unwrap_err();
        assert_eq!((id, error.code.as_str()), (Some(5), "bad_request"));
        // Settings a player may not change are not part of the protocol.
        let change =
            r#"{"id":6,"method":"set_settings","params":{"control_url":"https://evil.example"}}"#;
        assert!(parse_request(change).is_err());
        let long = format!(
            r#"{{"id":7,"method":"status","pad":"{}"}}"#,
            "x".repeat(MAX_LINE)
        );
        assert_eq!(parse_request(&long).unwrap_err().0, None);
    }

    #[test]
    fn messages_serialize_as_documented() {
        let welcome = Message::ok(
            1,
            Welcome {
                version: 1,
                service: "0.1.0".into(),
            },
        )
        .to_line();
        assert!(welcome.ends_with('\n'));
        assert_eq!(
            serde_json::from_str::<Value>(&welcome).unwrap(),
            serde_json::json!({"id": 1, "result": {"version": 1, "service": "0.1.0"}})
        );
        let error = ErrorBody::from(&EngineError::WarningNotAcknowledged);
        assert_eq!(error.code, "warning_not_acknowledged");
        let line = Message::err(3, error).to_line();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["error"]["code"], "warning_not_acknowledged");
        let event = Message::event(&EngineEvent::Status(Box::new(Status {
            mode: Mode::Measuring,
            ..Status::default()
        })));
        let value: Value = serde_json::from_str(&event.to_line()).unwrap();
        assert_eq!(value["event"], "status");
        assert_eq!(value["data"]["mode"], "measuring");
        // Lines parse back into messages.
        let back: Message = serde_json::from_str(line.trim()).unwrap();
        assert!(matches!(back, Message::Err { id: 3, .. }));
    }
}
