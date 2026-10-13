//! What the engine reports: its state, measurements, the live graph's
//! samples and notices for the player. All serialisable: the service sends
//! them to the UI over the named pipe (docs/decisions/007).

use serde::Serialize;

use crate::policy::Refusal;
use crate::select::Leg;

/// What the engine is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// No game selected.
    Idle,
    Measuring,
    /// Measured; the game goes direct and Optimize can be turned on.
    Ready,
    Connecting,
    Optimized,
    FailingOver,
}

/// The direct route to one of the game's regions.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RegionMeasurement {
    pub region: String,
    pub name: String,
    pub direct: Option<Leg>,
    /// Within reach of the player's best region, so one they would normally
    /// use (rule 3). Only these can be optimized.
    pub usual: bool,
}

/// The best way through PingPath, as estimated before connecting.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PathEstimate {
    pub exit: String,
    /// The entry relay, or `None` when going straight to the exit.
    pub entry: Option<String>,
    pub estimate_ms: f64,
}

/// A game and region, measured.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Measurement {
    pub game: String,
    pub region: String,
    pub regions: Vec<RegionMeasurement>,
    /// The selected region's direct route.
    pub direct: Option<Leg>,
    pub best: Option<PathEstimate>,
    /// Whether the estimate beats direct by the configured threshold.
    pub worthwhile: bool,
    /// The game is still being tested: the player must acknowledge a
    /// warning before optimizing (rule 7).
    pub needs_warning: bool,
    /// When the warning is another game's: a game still being tested whose
    /// servers this one shares, so its traffic would be carried too.
    pub warning_game: Option<String>,
    pub measured_at_unix: u64,
}

/// The path while optimized.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PathView {
    pub exit: String,
    pub entry: Option<String>,
    pub since_unix: u64,
    /// Measured through the tunnel when turned on: echo round trip plus the
    /// exit's round trip to the region.
    pub optimized_ms: Option<f64>,
    pub direct_ms: Option<f64>,
    /// The second entry while game traffic is duplicated (ADR 010).
    pub second_entry: Option<String>,
    /// Bytes sent and received as second copies since Optimize went on:
    /// the extra data duplication costs.
    pub extra_bytes_up: u64,
    pub extra_bytes_down: u64,
}

/// Everything the UI shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    pub mode: Mode,
    pub game: Option<String>,
    pub region: Option<String>,
    pub measurement: Option<Measurement>,
    pub path: Option<PathView>,
    /// Packets of the game carried through PingPath since Optimize went on.
    pub carried: u64,
    /// Game traffic dropped because the exit can't carry it, by reason.
    pub blocked: Vec<(String, u64)>,
    /// Notices the player hasn't dismissed yet, newest last.
    pub notices: Vec<Notice>,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            mode: Mode::Idle,
            game: None,
            region: None,
            measurement: None,
            path: None,
            carried: 0,
            blocked: Vec::new(),
            notices: Vec::new(),
        }
    }
}

/// Why optimization stopped by itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackCause {
    /// No relay path answered in time.
    RelaysUnreachable,
    /// The exit answered but stopped delivering the game's traffic.
    ExitNotForwarding,
    /// The session couldn't be renewed before its token ran out.
    SessionExpired,
    /// The session reached its maximum lifetime (12 hours).
    SessionEnded,
    /// The control plane refused to renew (for example, the game is no
    /// longer routed).
    RenewalRefused,
    /// The engine stopped responding and the guard removed the routes.
    EngineStalled,
    /// The PingPath adapter disappeared.
    AdapterLost,
    /// The PC went to sleep.
    Suspended,
}

/// Something the player must be told, in plain words (rule 6).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum NoticeKind {
    /// Optimization stopped and the game now connects directly; some games
    /// need to reconnect.
    FellBack {
        cause: FallbackCause,
    },
    /// The exit restarted, so the game server now sees new ports; some
    /// games need to reconnect.
    AddressChanged,
    /// Traffic moved to another entry relay with the same exit; the game
    /// server saw no change.
    FailedOver {
        entry: Option<String>,
    },
    /// Not turned on: the direct route is as good.
    NotBetter {
        direct_ms: f64,
        optimized_ms: f64,
    },
    /// Not turned on: the direct route couldn't be measured, so there's
    /// nothing to compare with.
    DirectUnknown,
    Refused {
        refusal: Refusal,
    },
    /// Some of the game's traffic is blocked while optimized because the
    /// exit can't carry it (not UDP, or other ports).
    GameTrafficBlocked {
        packets: u64,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Notice {
    pub id: u64,
    pub at_unix: u64,
    #[serde(flatten)]
    pub kind: NoticeKind,
}

/// One point on the live graph.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Sample {
    pub at_unix_ms: u64,
    pub optimized: bool,
    pub ping_ms: Option<f64>,
    pub jitter_ms: Option<f64>,
    pub loss_pct: f64,
}

/// Pushed to every subscriber.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum EngineEvent {
    Status(Box<Status>),
    Sample(Sample),
}

/// Why a request failed. `code` is stable, for the UI's translations.
#[derive(Debug, Clone, PartialEq, Serialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum EngineError {
    #[error("{refusal}")]
    Refused { refusal: Refusal },
    #[error("measure the game first")]
    NotMeasured,
    #[error("the direct route couldn't be measured, so there's nothing to compare with")]
    DirectUnknown,
    #[error("{region} isn't one of the regions you normally use (rule 3)")]
    NotUsualRegion { region: String },
    #[error("acknowledge the warning for games still being tested first")]
    WarningNotAcknowledged,
    #[error("no path through PingPath reaches this region yet")]
    NoPath,
    #[error("the direct route is as good ({direct_ms:.0} ms against {optimized_ms:.0} ms)")]
    NotBetter { direct_ms: f64, optimized_ms: f64 },
    #[error("busy: {doing}")]
    Busy { doing: String },
    #[error("{message}")]
    Control { message: String },
    #[error("couldn't connect: {message}")]
    Connect { message: String },
    #[error("network adapter or routes: {message}")]
    Platform { message: String },
    #[error("the engine stopped")]
    Stopped,
}
