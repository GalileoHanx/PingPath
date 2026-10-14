//! A whole doctor run (docs/decisions/011-home-network-doctor.md, "Where
//! the checks run"): the checks one after another, each result reported
//! as it ends, then the background check's traffic held against the
//! speeds the bufferbloat test measured.

use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::bufferbloat::{self, Plan};
use super::{
    CheckId, CheckResult, Finding, FindingCode, Severity, Status, background, connection, dns,
};
use crate::control::ControlClient;

/// Background traffic over this share of the measured download or upload
/// speed is worth a finding of its own on the bufferbloat result.
pub const BACKGROUND_SHARE: f64 = 0.25;

/// Every result, in the order the checks ran.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub results: Vec<CheckResult>,
    pub secs: f64,
}

/// What a run reports as it goes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Progress {
    Started { check: CheckId },
    Finished { result: CheckResult },
}

pub struct Options {
    pub control: Option<ControlClient>,
    /// A game is routed now: the bufferbloat test would make it lag.
    pub optimized: bool,
    pub plan: Plan,
    pub background_window: Duration,
}

impl Options {
    pub fn new(control: Option<ControlClient>, optimized: bool) -> Self {
        Self {
            control,
            optimized,
            plan: Plan::default(),
            background_window: background::WINDOW,
        }
    }
}

type Pending<'a> = Pin<Box<dyn Future<Output = CheckResult> + Send + 'a>>;

/// What the checks read, so tests can stand in for the machine.
pub trait System: Send + Sync + 'static {
    fn connection(&self) -> io::Result<connection::Figures>;
    fn background(&self, window: Duration) -> io::Result<background::Figures>;
    fn resolvers(&self) -> io::Result<Vec<IpAddr>>;
    fn dns(&self, resolvers: &[IpAddr]) -> dns::Figures;
    fn bufferbloat<'a>(&'a self, control: Option<ControlClient>, plan: &'a Plan) -> Pending<'a>;
}

/// The machine this runs on.
pub struct ThisComputer;

impl System for ThisComputer {
    fn connection(&self) -> io::Result<connection::Figures> {
        connection::read()
    }

    fn background(&self, window: Duration) -> io::Result<background::Figures> {
        background::read(window)
    }

    fn resolvers(&self) -> io::Result<Vec<IpAddr>> {
        dns::resolvers()
    }

    fn dns(&self, resolvers: &[IpAddr]) -> dns::Figures {
        dns::measure_all(resolvers)
    }

    fn bufferbloat<'a>(&'a self, control: Option<ControlClient>, plan: &'a Plan) -> Pending<'a> {
        Box::pin(bufferbloat::run(control, plan))
    }
}

impl CheckResult {
    /// Adds a finding; a result that has a status from its findings takes
    /// the new one's severity if it is worse.
    pub fn add(&mut self, finding: Finding) {
        let worse = match finding.severity {
            Severity::Problem => Some(Status::Problem),
            Severity::Warning => Some(Status::Warning),
            Severity::Info => None,
        };
        match (self.status, worse) {
            (Status::Good, Some(worse)) | (Status::Warning, Some(worse @ Status::Problem)) => {
                self.status = worse;
            }
            _ => {}
        }
        self.findings.push(finding);
    }
}

/// A finding on the bufferbloat result when background processes used a
/// large share of the speeds it measured.
pub fn background_share(bufferbloat: &mut CheckResult, background: &CheckResult) {
    let (Ok(test), Ok(traffic)) = (
        serde_json::from_value::<bufferbloat::Figures>(bufferbloat.figures.clone()),
        serde_json::from_value::<background::Figures>(background.figures.clone()),
    ) else {
        return;
    };
    let share = |kbps: f64, mbps: f64| {
        if mbps > 0.0 {
            kbps / 1000.0 / mbps
        } else {
            0.0
        }
    };
    let share = share(traffic.total_down_kbps, test.download.mbps)
        .max(share(traffic.total_up_kbps, test.upload.mbps));
    if share >= BACKGROUND_SHARE {
        bufferbloat.add(
            Finding::new(FindingCode::BackgroundShare, Severity::Warning)
                .with("pct", (share * 100.0).round().min(100.0)),
        );
    }
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| io::Error::other(e.to_string()))
}

/// Runs every check, reporting each as it starts and ends.
pub async fn run(
    system: Arc<dyn System>,
    options: Options,
    progress: impl Fn(Progress) + Send + Sync,
) -> Report {
    let started = Instant::now();
    tracing::info!(optimized = options.optimized, "network check started");
    let mut results = Vec::new();
    let finish = |result: CheckResult, results: &mut Vec<CheckResult>| {
        result.log();
        progress(Progress::Finished {
            result: result.clone(),
        });
        results.push(result);
    };

    progress(Progress::Started {
        check: CheckId::Connection,
    });
    let sys = system.clone();
    let result = connection::check(blocking(move || sys.connection()).await.and_then(|r| r));
    finish(result, &mut results);

    progress(Progress::Started {
        check: CheckId::Background,
    });
    let (sys, window) = (system.clone(), options.background_window);
    let result = background::check(
        blocking(move || sys.background(window))
            .await
            .and_then(|r| r),
    );
    finish(result, &mut results);

    progress(Progress::Started {
        check: CheckId::Dns,
    });
    let sys = system.clone();
    let result = match blocking(move || sys.resolvers().map(|resolvers| sys.dns(&resolvers)))
        .await
        .and_then(|r| r)
    {
        Ok(figures) => {
            let found = dns::findings(&figures);
            CheckResult::from_findings(CheckId::Dns, &figures, found)
        }
        Err(e) => CheckResult::from_error(CheckId::Dns, &e),
    };
    finish(result, &mut results);

    progress(Progress::Started {
        check: CheckId::Bufferbloat,
    });
    let mut result = if options.optimized {
        CheckResult::unavailable(
            CheckId::Bufferbloat,
            Finding::new(FindingCode::StopOptimizing, Severity::Info),
        )
    } else {
        system.bufferbloat(options.control, &options.plan).await
    };
    if let Some(background) = results.iter().find(|r| r.check == CheckId::Background) {
        background_share(&mut result, background);
    }
    finish(result, &mut results);

    let secs = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    tracing::info!(secs, "network check finished");
    Report { results, secs }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::background::{Kind, Process};
    use crate::doctor::bufferbloat::{Grade, Latency, Loaded};
    use crate::doctor::connection::Medium;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Fake {
        tested: AtomicBool,
        background_down_kbps: f64,
    }

    impl System for Fake {
        fn connection(&self) -> io::Result<connection::Figures> {
            Ok(connection::Figures {
                medium: Medium::Ethernet,
                adapter: Some("Ethernet".into()),
                is_virtual: false,
                link_mbps: Some(1000.0),
                wifi: None,
            })
        }

        fn background(&self, _window: Duration) -> io::Result<background::Figures> {
            Ok(background::Figures {
                window_secs: 10.0,
                processes: vec![Process {
                    name: "steam.exe".into(),
                    kind: Kind::Launcher,
                    down_kbps: self.background_down_kbps,
                    up_kbps: 50.0,
                }],
                total_down_kbps: self.background_down_kbps,
                total_up_kbps: 50.0,
            })
        }

        fn resolvers(&self) -> io::Result<Vec<IpAddr>> {
            Err(io::Error::new(io::ErrorKind::NotFound, "no resolver file"))
        }

        fn dns(&self, _resolvers: &[IpAddr]) -> dns::Figures {
            unreachable!("no resolvers were read")
        }

        fn bufferbloat<'a>(
            &'a self,
            _control: Option<ControlClient>,
            _plan: &'a Plan,
        ) -> Pending<'a> {
            self.tested.store(true, Ordering::Relaxed);
            let latency = |ms| Latency {
                median_ms: Some(ms),
                loss_pct: 0.0,
                pings: 70,
            };
            let figures = bufferbloat::Figures {
                server: "near".into(),
                idle: latency(20.0),
                download: Loaded {
                    latency: latency(22.0),
                    increase_ms: Some(2.0),
                    mbps: 40.0,
                },
                upload: Loaded {
                    latency: latency(21.0),
                    increase_ms: Some(1.0),
                    mbps: 10.0,
                },
                grade: Grade::APlus,
            };
            let found = bufferbloat::findings(&figures);
            Box::pin(
                async move { CheckResult::from_findings(CheckId::Bufferbloat, &figures, found) },
            )
        }
    }

    fn fake(background_down_kbps: f64) -> Arc<Fake> {
        Arc::new(Fake {
            tested: AtomicBool::new(false),
            background_down_kbps,
        })
    }

    fn options(optimized: bool) -> Options {
        Options {
            control: None,
            optimized,
            plan: Plan::default(),
            background_window: Duration::from_millis(1),
        }
    }

    #[tokio::test]
    async fn runs_the_checks_in_order_and_reports_each() {
        let system = fake(500.0);
        let seen = Mutex::new(Vec::new());
        let report = run(system.clone(), options(false), |p| {
            seen.lock().unwrap().push(p)
        })
        .await;
        let checks: Vec<CheckId> = report.results.iter().map(|r| r.check).collect();
        assert_eq!(
            checks,
            [
                CheckId::Connection,
                CheckId::Background,
                CheckId::Dns,
                CheckId::Bufferbloat
            ]
        );
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen.len(), 8);
        assert_eq!(
            seen[0],
            Progress::Started {
                check: CheckId::Connection
            }
        );
        assert!(
            matches!(&seen[1], Progress::Finished { result } if result.check == CheckId::Connection)
        );
        assert!(
            matches!(&seen[7], Progress::Finished { result } if result.check == CheckId::Bufferbloat)
        );
        // The resolvers couldn't be read: that check failed, the rest ran.
        assert_eq!(report.results[2].status, Status::Failed);
        assert_eq!(report.results[3].status, Status::Good);
        assert!(system.tested.load(Ordering::Relaxed));
        // 0.5 Mbit/s of 40 isn't worth a word.
        assert!(
            !report.results[3]
                .findings
                .iter()
                .any(|f| f.code == FindingCode::BackgroundShare)
        );
    }

    #[tokio::test]
    async fn no_bufferbloat_test_while_a_game_is_optimized() {
        let system = fake(500.0);
        let report = run(system.clone(), options(true), |_| {}).await;
        let test = &report.results[3];
        assert_eq!(test.status, Status::Unavailable);
        assert_eq!(test.findings[0].code, FindingCode::StopOptimizing);
        assert!(!system.tested.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn heavy_background_traffic_is_held_against_the_measured_speed() {
        // 15 Mbit/s of a 40 Mbit/s download.
        let report = run(fake(15_000.0), options(false), |_| {}).await;
        let test = &report.results[3];
        let share = test
            .findings
            .iter()
            .find(|f| f.code == FindingCode::BackgroundShare)
            .expect("a background share finding");
        assert_eq!(share.params["pct"], 38.0);
        assert_eq!(test.status, Status::Warning);
    }

    #[test]
    fn adding_a_finding_raises_the_status_only() {
        let mut result = CheckResult::from_findings(
            CheckId::Bufferbloat,
            &(),
            vec![Finding::new(
                FindingCode::BufferbloatGrade,
                Severity::Problem,
            )],
        );
        result.add(Finding::new(
            FindingCode::BackgroundShare,
            Severity::Warning,
        ));
        assert_eq!(result.status, Status::Problem);
        let mut unavailable = CheckResult::unavailable(
            CheckId::Bufferbloat,
            Finding::new(FindingCode::NoTestServer, Severity::Info),
        );
        unavailable.add(Finding::new(
            FindingCode::BackgroundShare,
            Severity::Warning,
        ));
        assert_eq!(unavailable.status, Status::Unavailable);
    }
}
