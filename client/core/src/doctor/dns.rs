//! DNS response time (docs/decisions/011-home-network-doctor.md, "DNS"):
//! the resolvers the internet connection uses, each asked for a few widely
//! used names twice (the second answer comes from its cache), against
//! three public resolvers for comparison.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use pingpath_probe::dns::{self, QueryType};
use serde::{Deserialize, Serialize};

use super::{CheckId, CheckResult, Finding, FindingCode, Severity};

/// Names most resolvers have cached or can find fast.
pub const NAMES: [&str; 5] = [
    "www.google.com",
    "www.microsoft.com",
    "store.steampowered.com",
    "www.youtube.com",
    "www.cloudflare.com",
];
/// For comparison only; the check works without them (rule 8).
pub const PUBLIC: [Ipv4Addr; 3] = dns::RESOLVERS;
pub const TIMEOUT: Duration = Duration::from_secs(2);
/// A resolver that left this many of its first questions unanswered isn't
/// asked the rest, so an unreachable one costs 4 s, not 20.
const GIVE_UP_AFTER: u32 = 2;

/// Slower than this from the cache: far away or overloaded.
pub const SLOW_CACHED_MS: f64 = 50.0;
/// Slower than this for the first answer.
pub const SLOW_FIRST_MS: f64 = 150.0;
/// More unanswered questions than this (of [`NAMES`]) is a problem.
pub const MAX_UNANSWERED: u32 = 1;
/// A public resolver is worth suggesting when it is at least this many
/// times faster for the first answer...
pub const FASTER_FACTOR: f64 = 2.0;
/// ...and at least this much faster.
pub const FASTER_BY_MS: f64 = 30.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolverFigures {
    pub address: IpAddr,
    /// Median of the first answers to [`NAMES`].
    pub first_ms: Option<f64>,
    /// Median of the second answers, from the resolver's cache.
    pub cached_ms: Option<f64>,
    /// Names asked for, and how many of those first questions went
    /// unanswered within [`TIMEOUT`].
    pub asked: u32,
    pub unanswered: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Figures {
    /// The player's, in the order the system uses them.
    pub resolvers: Vec<ResolverFigures>,
    pub public: Vec<ResolverFigures>,
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 0 {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    })
}

fn round(ms: f64) -> f64 {
    (ms * 10.0).round() / 10.0
}

/// Asks `server` for each name twice. `ask` answers with how long one
/// question took, or an error if it went unanswered.
pub fn measure(
    server: SocketAddr,
    names: &[&str],
    ask: impl Fn(SocketAddr, &str) -> io::Result<Duration>,
) -> ResolverFigures {
    let (mut first, mut cached) = (Vec::new(), Vec::new());
    let (mut asked, mut unanswered) = (0, 0);
    for name in names {
        asked += 1;
        if unanswered >= GIVE_UP_AFTER && unanswered + 1 == asked {
            // Nothing answered yet: count the rest as unanswered.
            unanswered += 1;
            continue;
        }
        match ask(server, name) {
            Ok(took) => first.push(took.as_secs_f64() * 1000.0),
            Err(_) => {
                unanswered += 1;
                continue;
            }
        }
        // A lost second answer only leaves a sample out.
        if let Ok(took) = ask(server, name) {
            cached.push(took.as_secs_f64() * 1000.0);
        }
    }
    ResolverFigures {
        address: server.ip(),
        first_ms: median(first).map(round),
        cached_ms: median(cached).map(round),
        asked,
        unanswered,
    }
}

/// One real question over UDP. A "no such name" answer is an answer.
pub fn ask(server: SocketAddr, name: &str) -> io::Result<Duration> {
    let start = Instant::now();
    dns::query(server, name, QueryType::A, TIMEOUT)?;
    Ok(start.elapsed())
}

/// Measures the player's resolvers one after another, the first first:
/// two of them are often the same router (its IPv6 and IPv4 address), and
/// asked at once, one's lookups would fill the cache the other's first
/// answers are timed against. The public resolvers, each its own cache,
/// are measured alongside, each on its own thread.
pub fn measure_all(resolvers: &[SocketAddr]) -> Figures {
    let public: Vec<SocketAddr> = PUBLIC
        .iter()
        .map(|ip| SocketAddr::new(IpAddr::V4(*ip), 53))
        .filter(|public| !resolvers.iter().any(|r| r.ip() == public.ip()))
        .collect();
    let unanswered = |server: SocketAddr| ResolverFigures {
        address: server.ip(),
        first_ms: None,
        cached_ms: None,
        asked: NAMES.len() as u32,
        unanswered: NAMES.len() as u32,
    };
    std::thread::scope(|scope| {
        let theirs: Vec<_> = public
            .iter()
            .map(|server| (*server, scope.spawn(move || measure(*server, &NAMES, ask))))
            .collect();
        let resolvers = resolvers
            .iter()
            .map(|server| measure(*server, &NAMES, ask))
            .collect();
        let public = theirs
            .into_iter()
            .map(|(server, handle)| handle.join().unwrap_or_else(|_| unanswered(server)))
            .collect();
        Figures { resolvers, public }
    })
}

pub fn findings(f: &Figures) -> Vec<Finding> {
    // Windows asks the first resolver, and the others only when it
    // doesn't answer.
    let Some(primary) = f.resolvers.first() else {
        return vec![Finding::new(FindingCode::DnsNone, Severity::Problem)];
    };
    let address = primary.address.to_string();
    let mut out = Vec::new();
    if primary.first_ms.is_none() || primary.unanswered > MAX_UNANSWERED {
        out.push(
            Finding::new(FindingCode::DnsUnanswered, Severity::Problem)
                .with("address", address.clone())
                .with("unanswered", primary.unanswered)
                .with("asked", primary.asked),
        );
    }
    if let Some(ms) = primary.cached_ms.filter(|ms| *ms > SLOW_CACHED_MS) {
        out.push(
            Finding::new(FindingCode::DnsSlowCached, Severity::Warning)
                .with("address", address.clone())
                .with("ms", ms.round()),
        );
    }
    if let Some(ms) = primary.first_ms.filter(|ms| *ms > SLOW_FIRST_MS) {
        out.push(
            Finding::new(FindingCode::DnsSlowFirst, Severity::Warning)
                .with("address", address.clone())
                .with("ms", ms.round()),
        );
    }
    let fastest = f
        .public
        .iter()
        .filter(|p| p.unanswered <= MAX_UNANSWERED)
        .filter_map(|p| Some((p, p.first_ms?)))
        .min_by(|a, b| a.1.total_cmp(&b.1));
    // A resolver that never answered has no time to compare; its finding
    // already says to set a public one.
    if let Some(((public, theirs), yours)) = fastest.zip(primary.first_ms) {
        if theirs * FASTER_FACTOR <= yours && yours - theirs >= FASTER_BY_MS {
            out.push(
                Finding::new(FindingCode::DnsFasterPublic, Severity::Info)
                    .with("address", public.address.to_string())
                    .with("ms", theirs.round())
                    .with("yours_ms", yours.round()),
            );
        }
    }
    if out.is_empty() {
        out.push(
            Finding::new(FindingCode::DnsGood, Severity::Info)
                .with("address", address)
                .with("ms", primary.first_ms.unwrap_or_default().round()),
        );
    }
    out
}

pub fn check(resolvers: io::Result<Vec<SocketAddr>>) -> CheckResult {
    match resolvers {
        Ok(resolvers) => {
            let figures = measure_all(&resolvers);
            let found = findings(&figures);
            CheckResult::from_findings(CheckId::Dns, &figures, found)
        }
        Err(e) => CheckResult::from_error(CheckId::Dns, &e),
    }
}

/// The resolvers the internet connection uses, in order, on port 53. A
/// link-local IPv6 one (a router's fe80:: address) keeps its scope, the
/// interface it is reached through.
pub fn resolvers() -> io::Result<Vec<SocketAddr>> {
    #[cfg(windows)]
    return super::windows::resolvers();
    #[cfg(target_os = "linux")]
    return std::fs::read_to_string("/etc/resolv.conf").map(|text| parse_resolv_conf(&text));
    #[cfg(not(any(windows, target_os = "linux")))]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the DNS check runs on Windows and Linux",
    ))
}

#[cfg(any(target_os = "linux", test))]
fn parse_resolv_conf(text: &str) -> Vec<SocketAddr> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            (words.next()? == "nameserver").then_some(())?;
            // A scoped IPv6 address (fe80::1%eth0) can't be asked here.
            let ip: IpAddr = words.next()?.parse().ok()?;
            Some(SocketAddr::new(ip, 53))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Status;
    use std::cell::Cell;

    fn resolver(
        address: &str,
        first: Option<f64>,
        cached: Option<f64>,
        unanswered: u32,
    ) -> ResolverFigures {
        ResolverFigures {
            address: address.parse().unwrap(),
            first_ms: first,
            cached_ms: cached,
            asked: 5,
            unanswered,
        }
    }

    fn codes(f: &Figures) -> Vec<FindingCode> {
        findings(f).iter().map(|f| f.code).collect()
    }

    #[test]
    fn measures_first_and_cached_answers() {
        let calls = Cell::new(0u32);
        let figures = measure("192.168.1.1:53".parse().unwrap(), &NAMES, |server, _| {
            assert_eq!(server.port(), 53);
            calls.set(calls.get() + 1);
            // First answers 40 ms, cached ones 4 ms; the third name goes
            // unanswered (and isn't asked again), the fourth's second
            // answer too.
            match calls.get() {
                5 | 7 => Err(io::Error::from(io::ErrorKind::TimedOut)),
                1 | 3 | 6 | 8 => Ok(Duration::from_millis(40)),
                _ => Ok(Duration::from_millis(4)),
            }
        });
        assert_eq!(calls.get(), 9);
        assert_eq!(figures.asked, 5);
        assert_eq!(figures.unanswered, 1);
        assert_eq!(figures.first_ms, Some(40.0));
        assert_eq!(figures.cached_ms, Some(4.0));
    }

    #[test]
    fn gives_up_on_a_resolver_that_never_answers() {
        let calls = Cell::new(0u32);
        let figures = measure("10.0.0.1:53".parse().unwrap(), &NAMES, |_, _| {
            calls.set(calls.get() + 1);
            Err(io::Error::from(io::ErrorKind::TimedOut))
        });
        assert_eq!(calls.get(), 2);
        assert_eq!((figures.asked, figures.unanswered), (5, 5));
        assert_eq!(figures.first_ms, None);
        let found = findings(&Figures {
            resolvers: vec![figures],
            public: vec![],
        });
        assert_eq!(found[0].code, FindingCode::DnsUnanswered);
        assert_eq!(found[0].params["unanswered"], 5);
        assert_eq!(found[0].params["asked"], 5);
    }

    #[test]
    fn a_resolver_that_never_answered_gets_no_time_compared() {
        let figures = Figures {
            resolvers: vec![resolver("192.168.1.1", None, None, 5)],
            public: vec![resolver("1.1.1.1", Some(20.0), Some(15.0), 0)],
        };
        // Not "1.1.1.1 answered in 20 ms, against 2000 ms for yours".
        assert_eq!(codes(&figures), [FindingCode::DnsUnanswered]);
    }

    #[test]
    fn a_fast_resolver_is_good() {
        let figures = Figures {
            resolvers: vec![resolver("192.168.1.1", Some(25.0), Some(3.0), 0)],
            public: vec![resolver("1.1.1.1", Some(20.0), Some(15.0), 0)],
        };
        assert_eq!(codes(&figures), [FindingCode::DnsGood]);
        assert_eq!(
            CheckResult::from_findings(CheckId::Dns, &figures, findings(&figures)).status,
            Status::Good
        );
    }

    #[test]
    fn a_slow_resolver_gets_a_faster_public_one_suggested() {
        let figures = Figures {
            resolvers: vec![resolver("10.20.30.40", Some(220.0), Some(80.0), 0)],
            public: vec![
                resolver("1.1.1.1", Some(30.0), Some(12.0), 0),
                resolver("8.8.8.8", Some(25.0), Some(14.0), 3),
                resolver("9.9.9.9", None, None, 10),
            ],
        };
        let found = findings(&figures);
        assert_eq!(
            found.iter().map(|f| f.code).collect::<Vec<_>>(),
            [
                FindingCode::DnsSlowCached,
                FindingCode::DnsSlowFirst,
                FindingCode::DnsFasterPublic
            ]
        );
        // 8.8.8.8 was faster but lost answers.
        assert_eq!(found[2].params["address"], "1.1.1.1");
        assert_eq!(found[2].params["yours_ms"], 220.0);
        assert_eq!(
            CheckResult::from_findings(CheckId::Dns, &figures, found).status,
            Status::Warning
        );
    }

    #[test]
    fn a_small_gain_isnt_worth_a_change() {
        // Twice as fast, but only 20 ms.
        let figures = Figures {
            resolvers: vec![resolver("192.168.1.1", Some(40.0), Some(5.0), 0)],
            public: vec![resolver("1.1.1.1", Some(20.0), Some(10.0), 0)],
        };
        assert_eq!(codes(&figures), [FindingCode::DnsGood]);
    }

    #[test]
    fn losing_answers_is_a_problem_and_no_resolver_at_all_too() {
        let figures = Figures {
            resolvers: vec![resolver("192.168.1.1", Some(30.0), Some(5.0), 4)],
            public: vec![],
        };
        assert_eq!(codes(&figures), [FindingCode::DnsUnanswered]);
        let none = Figures {
            resolvers: vec![],
            public: vec![],
        };
        assert_eq!(codes(&none), [FindingCode::DnsNone]);
        assert_eq!(
            CheckResult::from_findings(CheckId::Dns, &none, findings(&none)).status,
            Status::Problem
        );
    }

    #[test]
    fn reads_resolv_conf() {
        let text = "# generated\nsearch lan\nnameserver 192.168.1.1\nnameserver 2001:db8::53\nnameserver fe80::1%eth0\noptions edns0\n";
        assert_eq!(
            parse_resolv_conf(text),
            [
                "192.168.1.1:53".parse::<SocketAddr>().unwrap(),
                "[2001:db8::53]:53".parse().unwrap()
            ]
        );
    }

    #[test]
    fn medians() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), Some(2.5));
    }
}
