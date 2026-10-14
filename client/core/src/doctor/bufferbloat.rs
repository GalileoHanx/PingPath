//! The bufferbloat test (docs/decisions/011-home-network-doctor.md,
//! "Bufferbloat"): latency to the nearest relay with a doctor listener at
//! idle, then while it downloads over eight TCP connections, then while it
//! uploads over eight, graded on the larger increase with Waveform's
//! thresholds.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pingpath_tunnel::ticket::{ACCEPTED, DOWN, UP};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep, sleep_until, timeout};

use super::{CheckId, CheckResult, Finding, FindingCode, Severity};
use crate::control::{ControlClient, ControlError};
use crate::measure::{ProbePlan, probe_relays};

/// A loaded phase that lost more than this share of its pings is graded F:
/// the queue was so long that they timed out.
pub const MAX_LOADED_LOSS_PCT: f64 = 50.0;
/// A loaded phase needs this many counted pings to say anything; fewer
/// when the ticket's byte budget ran out early (a line faster than the
/// test can fill).
pub const MIN_LOADED_PINGS: u32 = 5;
/// SQM is suggested at this share of the measured speeds, so that the
/// router's queue, not the ISP's, is the one that fills.
pub const SQM_SHARE: f64 = 0.9;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The connection that keeps the relay's echo answering writes a byte this
/// often, well inside the relay's idle timeout.
const KEEPER_INTERVAL: Duration = Duration::from_secs(2);
const CHUNK: usize = 64 * 1024;

/// Waveform's bufferbloat grades, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Grade {
    #[serde(rename = "A+")]
    APlus,
    A,
    B,
    C,
    D,
    F,
}

impl Grade {
    /// From the latency added under load, in ms.
    pub fn from_increase(ms: f64) -> Self {
        match ms {
            ms if ms < 5.0 => Self::APlus,
            ms if ms < 30.0 => Self::A,
            ms if ms < 60.0 => Self::B,
            ms if ms < 200.0 => Self::C,
            ms if ms < 400.0 => Self::D,
            _ => Self::F,
        }
    }

    /// How many letters apart two grades are (A+ is next to A).
    pub fn distance(self, other: Self) -> u8 {
        (self as i8 - other as i8).unsigned_abs()
    }

    pub fn letter(self) -> &'static str {
        match self {
            Self::APlus => "A+",
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
            Self::D => "D",
            Self::F => "F",
        }
    }
}

/// How long each part of the test lasts.
#[derive(Debug, Clone)]
pub struct Plan {
    pub idle: Duration,
    /// Each loaded phase.
    pub loaded: Duration,
    /// The start of a loaded phase that isn't counted: TCP speeding up.
    pub warmup: Duration,
    /// TCP connections per loaded phase.
    pub flows: usize,
    pub ping_interval: Duration,
    /// How long after the last phase late answers still count.
    pub grace: Duration,
}

impl Default for Plan {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(5),
            loaded: Duration::from_secs(8),
            warmup: Duration::from_secs(1),
            flows: 8,
            ping_interval: Duration::from_millis(100),
            grace: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Latency {
    pub median_ms: Option<f64>,
    pub loss_pct: f64,
    pub pings: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Loaded {
    #[serde(flatten)]
    pub latency: Latency,
    /// The loaded median minus the idle one.
    pub increase_ms: Option<f64>,
    /// What the TCP connections reached.
    pub mbps: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Figures {
    /// The relay the test ran against.
    pub server: String,
    pub idle: Latency,
    pub download: Loaded,
    pub upload: Loaded,
    pub grade: Grade,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Download,
    Upload,
    Done,
}

#[derive(Debug, Clone)]
struct Ping {
    phase: Phase,
    /// Sent after the phase's warmup (always, at idle).
    counted: bool,
    sent: Instant,
    rtt: Option<Duration>,
}

fn round1(ms: f64) -> f64 {
    (ms * 10.0).round() / 10.0
}

fn summarize(pings: &[Ping], phase: Phase) -> Latency {
    let counted: Vec<&Ping> = pings
        .iter()
        .filter(|p| p.phase == phase && p.counted)
        .collect();
    let mut rtts: Vec<f64> = counted
        .iter()
        .filter_map(|p| p.rtt.map(|d| d.as_secs_f64() * 1000.0))
        .collect();
    rtts.sort_by(f64::total_cmp);
    let median = (!rtts.is_empty()).then(|| {
        let mid = rtts.len() / 2;
        if rtts.len() % 2 == 0 {
            (rtts[mid - 1] + rtts[mid]) / 2.0
        } else {
            rtts[mid]
        }
    });
    let sent = counted.len() as u32;
    Latency {
        median_ms: median.map(round1),
        loss_pct: if sent == 0 {
            0.0
        } else {
            round1(100.0 * f64::from(sent - rtts.len() as u32) / f64::from(sent))
        },
        pings: sent,
    }
}

/// The figures and the grade, from the three phases.
pub fn grade(
    server: String,
    idle: Latency,
    download: (Latency, f64),
    upload: (Latency, f64),
) -> Figures {
    let loaded = |(latency, mbps): (Latency, f64)| Loaded {
        increase_ms: latency
            .median_ms
            .zip(idle.median_ms)
            .map(|(loaded, idle)| round1((loaded - idle).max(0.0))),
        latency,
        mbps: round1(mbps),
    };
    let (download, upload) = (loaded(download), loaded(upload));
    let measured = |l: &Loaded| l.latency.pings >= MIN_LOADED_PINGS;
    let timed_out = |l: &Loaded| {
        measured(l) && (l.latency.loss_pct > MAX_LOADED_LOSS_PCT || l.increase_ms.is_none())
    };
    let grade = if timed_out(&download) || timed_out(&upload) {
        Grade::F
    } else {
        Grade::from_increase(
            [&download, &upload]
                .into_iter()
                .filter(|l| measured(l))
                .filter_map(|l| l.increase_ms)
                .fold(0.0, f64::max),
        )
    };
    Figures {
        server,
        idle,
        download,
        upload,
        grade,
    }
}

pub fn findings(f: &Figures) -> Vec<Finding> {
    let severity = match f.grade {
        Grade::APlus | Grade::A => Severity::Info,
        Grade::B => Severity::Warning,
        Grade::C | Grade::D | Grade::F => Severity::Problem,
    };
    let increase = |l: &Loaded| {
        l.increase_ms
            .map_or(serde_json::Value::Null, |ms| ms.round().into())
    };
    let mut out = vec![
        Finding::new(FindingCode::BufferbloatGrade, severity)
            .with("grade", f.grade.letter())
            .with("download_ms", increase(&f.download))
            .with("upload_ms", increase(&f.upload)),
    ];
    if f.grade >= Grade::C {
        out.push(
            Finding::new(FindingCode::SqmSuggested, Severity::Info)
                .with("download_mbps", (f.download.mbps * SQM_SHARE).floor())
                .with("upload_mbps", (f.upload.mbps * SQM_SHARE).floor()),
        );
    }
    out
}

/// Opens a load connection with the ticket.
async fn open(server: SocketAddr, ticket: &[u8], direction: u8) -> io::Result<TcpStream> {
    let mut stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(server))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the test server didn't answer"))??;
    stream.set_nodelay(true)?;
    let len = u16::try_from(ticket.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ticket too long"))?;
    let mut request = len.to_be_bytes().to_vec();
    request.extend_from_slice(ticket);
    request.push(direction);
    stream.write_all(&request).await?;
    let answer = timeout(CONNECT_TIMEOUT, stream.read_u8())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the test server didn't answer"))??;
    if answer != ACCEPTED {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the test server refused the ticket",
        ));
    }
    Ok(stream)
}

/// One loaded phase: `flows` connections in `direction` for `plan.loaded`;
/// the Mbit/s they reached after the warmup.
///
/// Upload counts what the relay reports receiving: what the client wrote
/// includes what still sits in its socket buffers, which grow through the
/// phase (about 8% over the line's rate with a 600 ms queue).
async fn load(server: SocketAddr, ticket: &[u8], direction: u8, plan: &Plan) -> io::Result<f64> {
    let start = Instant::now();
    let counted_from = start + plan.warmup;
    let deadline = start + plan.loaded;
    // Bytes after the warmup, and all of them.
    let bytes = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(0));
    // What the relay received after the warmup, by its reports (upload).
    let reported = Arc::new(AtomicU64::new(0));
    let has_reports = Arc::new(AtomicBool::new(false));
    let opened = Arc::new(AtomicUsize::new(0));
    let mut flows = JoinSet::new();
    for _ in 0..plan.flows {
        let (ticket, bytes, total, reported, has_reports, opened) = (
            ticket.to_vec(),
            bytes.clone(),
            total.clone(),
            reported.clone(),
            has_reports.clone(),
            opened.clone(),
        );
        flows.spawn(async move {
            let mut stream = open(server, &ticket, direction).await?;
            opened.fetch_add(1, Ordering::Relaxed);
            let count = |n: usize| {
                total.fetch_add(n as u64, Ordering::Relaxed);
                if Instant::now() >= counted_from {
                    bytes.fetch_add(n as u64, Ordering::Relaxed);
                }
            };
            let mut buffer = vec![0x5a_u8; CHUNK];
            if direction == DOWN {
                loop {
                    let n = stream.read(&mut buffer).await?;
                    if n == 0 {
                        return Ok::<(), io::Error>(());
                    }
                    count(n);
                }
            }
            let (mut reader, mut writer) = stream.split();
            let writes = async {
                loop {
                    let n = writer.write(&buffer).await?;
                    if n == 0 {
                        return Ok(());
                    }
                    count(n);
                }
            };
            // The relay's running count. What arrived since the last
            // report before the warmup ended is counted, so the reports
            // span about as long as the phase after its warmup.
            let reports = async {
                let mut last = None;
                let mut report = [0u8; 8];
                loop {
                    reader.read_exact(&mut report).await?;
                    let received = u64::from_be_bytes(report);
                    if let Some(last) = last.filter(|_| Instant::now() >= counted_from) {
                        reported.fetch_add(received.saturating_sub(last), Ordering::Relaxed);
                        has_reports.store(true, Ordering::Relaxed);
                    }
                    last = Some(received);
                }
            };
            tokio::select! {
                done = writes => done,
                done = reports => done,
            }
        });
    }
    // Errors before the deadline (a refused or reset connection) leave the
    // others to carry the load.
    let mut first_error = None;
    while let Ok(Some(done)) = timeout(deadline - Instant::now(), flows.join_next()).await {
        if let Ok(Err(e)) = done {
            first_error.get_or_insert(e);
        }
    }
    flows.abort_all();
    // Every connection ends early when the ticket's budget runs out.
    let end = Instant::now().min(deadline);
    if opened.load(Ordering::Relaxed) == 0 {
        return Err(first_error.unwrap_or_else(|| io::Error::other("no load connection opened")));
    }
    let mbps = |bytes: u64, from: Instant| {
        bytes as f64 * 8.0 / (end - from).as_secs_f64().max(0.001) / 1e6
    };
    let counted = if has_reports.load(Ordering::Relaxed) {
        reported.load(Ordering::Relaxed)
    } else {
        bytes.load(Ordering::Relaxed)
    };
    Ok(if end >= counted_from + plan.warmup / 2 {
        mbps(counted, counted_from)
    } else {
        // Over before the warmup was: all of it is all there is.
        mbps(total.load(Ordering::Relaxed), start)
    })
}

/// Runs the three phases against `server` with a ticket for it.
pub async fn measure(
    server: SocketAddr,
    ticket: &[u8],
    plan: &Plan,
) -> io::Result<(Latency, (Latency, f64), (Latency, f64))> {
    // An open connection is what lets the relay's echo answer; this one
    // stays open through all three phases.
    let mut keeper = open(server, ticket, UP).await?;
    let keeper = tokio::spawn(async move {
        loop {
            sleep(KEEPER_INTERVAL).await;
            if keeper.write_all(&[0]).await.is_err() {
                break;
            }
        }
    });

    let local: IpAddr = if server.is_ipv4() {
        Ipv4Addr::UNSPECIFIED.into()
    } else {
        Ipv6Addr::UNSPECIFIED.into()
    };
    let socket = Arc::new(UdpSocket::bind((local, 0)).await?);
    socket.connect(server).await?;
    let tag: [u8; 8] = pingpath_tunnel::keys::random_32()
        .map_err(|e| io::Error::other(e.to_string()))?[..8]
        .try_into()
        .expect("8 bytes");
    let pings: Arc<Mutex<Vec<Ping>>> = Arc::default();
    let phase = Arc::new(Mutex::new((Phase::Idle, Instant::now())));

    let sender = {
        let (socket, pings, phase, plan) =
            (socket.clone(), pings.clone(), phase.clone(), plan.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(plan.ping_interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let (now_phase, since) = *phase.lock().expect("phase lock");
                if now_phase == Phase::Done {
                    break;
                }
                let seq = {
                    let mut pings = pings.lock().expect("pings lock");
                    pings.push(Ping {
                        phase: now_phase,
                        counted: now_phase == Phase::Idle || since.elapsed() >= plan.warmup,
                        sent: Instant::now(),
                        rtt: None,
                    });
                    pings.len() as u32 - 1
                };
                let mut payload = seq.to_be_bytes().to_vec();
                payload.extend_from_slice(&tag);
                // A ping that can't be sent is a lost ping.
                let _ = socket.send(&payload).await;
            }
        })
    };
    let receiver = {
        let (socket, pings) = (socket.clone(), pings.clone());
        tokio::spawn(async move {
            let mut buffer = [0u8; 64];
            loop {
                let Ok(n) = socket.recv(&mut buffer).await else {
                    // ICMP errors surface here; the pings stay unanswered.
                    continue;
                };
                if n != 12 || buffer[4..12] != tag {
                    continue;
                }
                let seq = u32::from_be_bytes(buffer[..4].try_into().expect("4 bytes")) as usize;
                if let Some(ping) = pings.lock().expect("pings lock").get_mut(seq) {
                    ping.rtt.get_or_insert_with(|| ping.sent.elapsed());
                }
            }
        })
    };

    let set_phase = |next| *phase.lock().expect("phase lock") = (next, Instant::now());
    sleep(plan.idle).await;
    set_phase(Phase::Download);
    let download = load(server, ticket, DOWN, plan).await;
    set_phase(Phase::Upload);
    let upload = match &download {
        Ok(_) => load(server, ticket, UP, plan).await,
        Err(_) => Err(io::Error::other("skipped")),
    };
    set_phase(Phase::Done);
    sleep_until(Instant::now() + plan.grace).await;
    sender.abort();
    receiver.abort();
    keeper.abort();
    let (download, upload) = (download?, upload?);

    let pings = pings.lock().expect("pings lock").clone();
    let idle = summarize(&pings, Phase::Idle);
    if idle.median_ms.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the test server's echo never answered (UDP to it may be blocked)",
        ));
    }
    let (down_latency, up_latency) = (
        summarize(&pings, Phase::Download),
        summarize(&pings, Phase::Upload),
    );
    if down_latency.pings < MIN_LOADED_PINGS && up_latency.pings < MIN_LOADED_PINGS {
        return Err(io::Error::other(format!(
            "the test's byte budget ran out before latency under load could be measured \
             ({download:.0} Mbit/s down, {upload:.0} up): the line is faster than the test can fill"
        )));
    }
    Ok((idle, (down_latency, download), (up_latency, upload)))
}

/// Why the test didn't produce a grade.
enum NotRun {
    Unavailable(Finding),
    Failed(io::Error),
}

impl From<io::Error> for NotRun {
    fn from(e: io::Error) -> Self {
        Self::Failed(e)
    }
}

fn control_error(e: ControlError) -> NotRun {
    match e {
        ControlError::Refused {
            code, retry_after, ..
        } if code == "doctor_limit" => NotRun::Unavailable(
            Finding::new(FindingCode::TestLimitReached, Severity::Info)
                .with("minutes", retry_after.map_or(60, |s| s.div_ceil(60))),
        ),
        // That relay ran its share of tests this hour: no server for now.
        ControlError::Refused { code, .. } if code == "doctor_busy" => {
            NotRun::Unavailable(Finding::new(FindingCode::NoTestServer, Severity::Info))
        }
        ControlError::NotConfigured => {
            NotRun::Unavailable(Finding::new(FindingCode::NoTestServer, Severity::Info))
        }
        other => NotRun::Failed(io::Error::other(other.to_string())),
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ControlError> + Send + 'static,
) -> Result<T, NotRun> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| NotRun::Failed(io::Error::other(e.to_string())))?
        .map_err(control_error)
}

async fn try_run(control: Option<ControlClient>, plan: &Plan) -> Result<Figures, NotRun> {
    let control = control.ok_or_else(|| {
        NotRun::Unavailable(Finding::new(FindingCode::NoTestServer, Severity::Info))
    })?;
    let list = control.clone();
    let relays = blocking(move || list.relays()).await?;
    let candidates: Vec<_> = relays.iter().filter(|r| r.doctor.is_some()).collect();
    if candidates.is_empty() {
        return Err(NotRun::Unavailable(Finding::new(
            FindingCode::NoTestServer,
            Severity::Info,
        )));
    }
    // The nearest by the PROBE round trip the engine measures too.
    let targets: Vec<(u32, SocketAddr)> = candidates.iter().map(|r| (r.id, r.address)).collect();
    let legs = probe_relays(
        &targets,
        ProbePlan {
            probes: 3,
            spacing: Duration::from_millis(50),
            timeout: Duration::from_secs(1),
        },
    )
    .await;
    let chosen = candidates
        .iter()
        .min_by(|a, b| {
            let rtt = |id| legs.get(&id).map_or(f64::INFINITY, |leg| leg.rtt_ms);
            rtt(a.id).total_cmp(&rtt(b.id))
        })
        .expect("at least one candidate");
    tracing::info!(relay = %chosen.name, rtt_ms = ?legs.get(&chosen.id).map(|l| l.rtt_ms), "bufferbloat test server chosen");
    let relay_id = chosen.id;
    let ticket = blocking(move || control.doctor_ticket(relay_id)).await?;
    let bytes = ticket
        .ticket_bytes()
        .map_err(|e| NotRun::Failed(io::Error::other(e.to_string())))?;
    let (idle, download, upload) = measure(ticket.address, &bytes, plan).await?;
    Ok(grade(chosen.name.clone(), idle, download, upload))
}

/// The whole check: picks the server, gets a ticket, measures and grades.
pub async fn run(control: Option<ControlClient>, plan: &Plan) -> CheckResult {
    match try_run(control, plan).await {
        Ok(figures) => {
            let found = findings(&figures);
            CheckResult::from_findings(CheckId::Bufferbloat, &figures, found)
        }
        Err(NotRun::Unavailable(why)) => CheckResult::unavailable(CheckId::Bufferbloat, why),
        Err(NotRun::Failed(e)) => CheckResult::from_error(CheckId::Bufferbloat, &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Status;

    #[test]
    fn grades_follow_waveforms_thresholds() {
        let cases = [
            (0.0, Grade::APlus),
            (4.9, Grade::APlus),
            (5.0, Grade::A),
            (29.9, Grade::A),
            (30.0, Grade::B),
            (59.0, Grade::B),
            (60.0, Grade::C),
            (199.0, Grade::C),
            (200.0, Grade::D),
            (399.0, Grade::D),
            (400.0, Grade::F),
            (2000.0, Grade::F),
        ];
        for (ms, grade) in cases {
            assert_eq!(Grade::from_increase(ms), grade, "{ms} ms");
        }
        assert_eq!(Grade::APlus.distance(Grade::A), 1);
        assert_eq!(Grade::C.distance(Grade::A), 2);
        assert_eq!(Grade::F.distance(Grade::F), 0);
        assert_eq!(serde_json::to_value(Grade::APlus).unwrap(), "A+");
    }

    fn latency(median: Option<f64>, loss: f64) -> Latency {
        Latency {
            median_ms: median,
            loss_pct: loss,
            pings: 50,
        }
    }

    #[test]
    fn the_larger_increase_decides() {
        let figures = grade(
            "relay".into(),
            latency(Some(20.0), 0.0),
            (latency(Some(45.0), 0.0), 95.0),
            (latency(Some(140.0), 2.0), 9.5),
        );
        assert_eq!(figures.download.increase_ms, Some(25.0));
        assert_eq!(figures.upload.increase_ms, Some(120.0));
        assert_eq!(figures.grade, Grade::C);
        let found = findings(&figures);
        assert_eq!(found[0].code, FindingCode::BufferbloatGrade);
        assert_eq!(found[0].params["grade"], "C");
        assert_eq!(found[0].severity, Severity::Problem);
        assert_eq!(found[1].code, FindingCode::SqmSuggested);
        assert_eq!(found[1].params["download_mbps"], 85.0);
        assert_eq!(found[1].params["upload_mbps"], 8.0);
        assert_eq!(
            CheckResult::from_findings(CheckId::Bufferbloat, &figures, found).status,
            Status::Problem
        );
    }

    #[test]
    fn a_good_line_needs_no_sqm_and_a_lossy_one_fails() {
        let good = grade(
            "relay".into(),
            latency(Some(20.0), 0.0),
            (latency(Some(22.0), 0.0), 95.0),
            (latency(Some(19.0), 0.0), 20.0),
        );
        assert_eq!(good.grade, Grade::APlus);
        assert_eq!(good.upload.increase_ms, Some(0.0));
        assert_eq!(findings(&good).len(), 1);
        assert_eq!(findings(&good)[0].severity, Severity::Info);
        let b = grade(
            "relay".into(),
            latency(Some(20.0), 0.0),
            (latency(Some(65.0), 0.0), 95.0),
            (latency(Some(30.0), 0.0), 20.0),
        );
        assert_eq!(b.grade, Grade::B);
        assert_eq!(findings(&b)[0].severity, Severity::Warning);
        let timed_out = grade(
            "relay".into(),
            latency(Some(20.0), 0.0),
            (latency(Some(80.0), 60.0), 95.0),
            (latency(None, 100.0), 20.0),
        );
        assert_eq!(timed_out.grade, Grade::F);
        assert_eq!(
            findings(&timed_out)[0].params["upload_ms"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn a_phase_cut_short_by_the_budget_isnt_graded() {
        let short = Latency {
            median_ms: None,
            loss_pct: 0.0,
            pings: 2,
        };
        let figures = grade(
            "relay".into(),
            latency(Some(20.0), 0.0),
            (short, 900.0),
            (latency(Some(70.0), 0.0), 40.0),
        );
        assert_eq!(figures.download.increase_ms, None);
        assert_eq!(figures.grade, Grade::B);
    }

    #[test]
    fn pings_in_a_warmup_dont_count() {
        let now = Instant::now();
        let ping = |phase, counted, rtt: Option<u64>| Ping {
            phase,
            counted,
            sent: now,
            rtt: rtt.map(Duration::from_millis),
        };
        let pings = vec![
            ping(Phase::Idle, true, Some(10)),
            ping(Phase::Idle, true, Some(12)),
            ping(Phase::Idle, true, None),
            ping(Phase::Download, false, Some(500)),
            ping(Phase::Download, true, Some(80)),
            ping(Phase::Download, true, Some(90)),
        ];
        let idle = summarize(&pings, Phase::Idle);
        assert_eq!(idle.median_ms, Some(11.0));
        assert_eq!(idle.pings, 3);
        assert_eq!(idle.loss_pct, 33.3);
        let download = summarize(&pings, Phase::Download);
        assert_eq!(download.median_ms, Some(85.0));
        assert_eq!(download.loss_pct, 0.0);
        assert_eq!(summarize(&pings, Phase::Upload), Latency::default());
    }
}
