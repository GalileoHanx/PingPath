//! Turns a region's measurements into a plain-language verdict. The
//! thresholds are heuristics; the report always shows the numbers behind
//! them, so a verdict can be second-guessed.

use serde::Serialize;

use crate::stats::Stats;

/// First hop (the player's router) slower than this suggests Wi-Fi or a
/// busy home connection.
const HOME_HOP_MS: f64 = 10.0;
const HOME_HOP_JITTER_MS: f64 = 5.0;
const HOME_HOP_LOSS_PCT: f64 = 2.0;
/// Latency above the distance floor by more than this (and by more than
/// `EXCESS_SHARE` of the floor) points at the route.
const EXCESS_MS: f64 = 25.0;
const EXCESS_SHARE: f64 = 0.4;
const PATH_LOSS_PCT: f64 = 0.5;
const PATH_JITTER_MS: f64 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    /// The problem starts inside the player's home (Wi-Fi, router, load).
    HomeNetwork,
    /// Much slower than the distance explains: a detour or slow route.
    IspRoute,
    /// Loss or jitter on the way, beyond the home network.
    PathLoss,
    NearOptimal,
    Unreachable,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub kind: VerdictKind,
    pub summary: String,
    pub reasons: Vec<String>,
}

pub struct Inputs<'a> {
    /// The best target in the region.
    pub best: Option<&'a Stats>,
    pub floor_ms: Option<f64>,
    /// The first hop that answered in the traceroute, if one ran.
    pub first_hop: Option<&'a Stats>,
}

pub fn judge(inputs: &Inputs) -> Verdict {
    let Some(best) = inputs.best else {
        return verdict(
            VerdictKind::Unknown,
            "No target in this region could be measured.",
            vec![],
        );
    };
    let Some(avg) = best.avg_ms else {
        return verdict(
            VerdictKind::Unreachable,
            "No replies from any target in this region.",
            vec![],
        );
    };

    let mut findings: Vec<(VerdictKind, String)> = Vec::new();
    if let Some(hop) = inputs.first_hop {
        let slow = hop.avg_ms.is_some_and(|ms| ms > HOME_HOP_MS);
        let jittery = hop.jitter_ms.is_some_and(|ms| ms > HOME_HOP_JITTER_MS);
        let lossy = hop.received > 0 && hop.loss_pct >= HOME_HOP_LOSS_PCT;
        if slow || jittery || lossy {
            findings.push((
                VerdictKind::HomeNetwork,
                format!(
                    "Your router (first hop) answers in {} ms on average with {} ms jitter and {}% loss; \
                     it should be under {HOME_HOP_MS} ms with almost no jitter. Usually Wi-Fi or other \
                     traffic on your connection.",
                    fmt(hop.avg_ms),
                    fmt(hop.jitter_ms),
                    hop.loss_pct
                ),
            ));
        }
    }
    if let Some(floor) = inputs.floor_ms {
        let excess = avg - floor;
        if excess > EXCESS_MS && excess > EXCESS_SHARE * floor {
            findings.push((
                VerdictKind::IspRoute,
                format!(
                    "{avg:.1} ms average is {excess:.1} ms above the {floor:.1} ms that the distance \
                     allows: the route takes a detour or a slow path."
                ),
            ));
        }
    }
    let jitter = best.jitter_ms.unwrap_or(0.0);
    if best.loss_pct > PATH_LOSS_PCT || jitter > PATH_JITTER_MS {
        findings.push((
            VerdictKind::PathLoss,
            format!(
                "{}% loss and {jitter:.1} ms jitter to the server.",
                best.loss_pct
            ),
        ));
    }

    let priority = [
        VerdictKind::HomeNetwork,
        VerdictKind::IspRoute,
        VerdictKind::PathLoss,
    ];
    let primary = priority
        .iter()
        .find(|k| findings.iter().any(|(f, _)| f == *k))
        .copied();
    let reasons: Vec<String> = findings.into_iter().map(|(_, r)| r).collect();
    match primary {
        Some(VerdictKind::HomeNetwork) => verdict(
            VerdictKind::HomeNetwork,
            "Problem inside your home network.",
            reasons,
        ),
        Some(VerdictKind::IspRoute) => verdict(
            VerdictKind::IspRoute,
            "Your ISP's route is slow: PingPath can help here.",
            reasons,
        ),
        Some(_) => verdict(
            VerdictKind::PathLoss,
            "Loss or jitter on the way: Stability mode can help here.",
            reasons,
        ),
        None if inputs.floor_ms.is_some() => verdict(
            VerdictKind::NearOptimal,
            "Close to the best the distance allows: little to gain.",
            vec![format!(
                "{avg:.1} ms average, {}% loss, {jitter:.1} ms jitter.",
                best.loss_pct
            )],
        ),
        None => verdict(
            VerdictKind::Unknown,
            "Looks healthy, but your location is unknown so the distance floor can't be checked; pass --from <city>.",
            vec![format!(
                "{avg:.1} ms average, {}% loss, {jitter:.1} ms jitter.",
                best.loss_pct
            )],
        ),
    }
}

fn verdict(kind: VerdictKind, summary: &str, reasons: Vec<String>) -> Verdict {
    Verdict {
        kind,
        summary: summary.to_string(),
        reasons,
    }
}

fn fmt(ms: Option<f64>) -> String {
    ms.map_or("?".into(), |v| format!("{v:.1}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn stats(rtts: &[Option<u64>]) -> Stats {
        let samples: Vec<_> = rtts.iter().map(|r| r.map(Duration::from_millis)).collect();
        Stats::from_samples(&samples)
    }

    #[test]
    fn slow_route_is_an_isp_problem() {
        let best = stats(&[Some(96), Some(97), Some(96), Some(98)]);
        let hop = stats(&[Some(1), Some(1), Some(2), Some(1)]);
        let v = judge(&Inputs {
            best: Some(&best),
            floor_ms: Some(43.1),
            first_hop: Some(&hop),
        });
        assert_eq!(v.kind, VerdictKind::IspRoute);
    }

    #[test]
    fn slow_router_is_a_home_problem_first() {
        let best = stats(&[Some(96), Some(130), Some(96), None]);
        let hop = stats(&[Some(4), Some(25), Some(3), Some(30)]);
        let v = judge(&Inputs {
            best: Some(&best),
            floor_ms: Some(43.1),
            first_hop: Some(&hop),
        });
        assert_eq!(v.kind, VerdictKind::HomeNetwork);
        assert_eq!(v.reasons.len(), 3, "{:?}", v.reasons);
    }

    #[test]
    fn close_to_the_floor_is_near_optimal() {
        let best = stats(&[Some(55), Some(56), Some(55), Some(56)]);
        let v = judge(&Inputs {
            best: Some(&best),
            floor_ms: Some(43.1),
            first_hop: None,
        });
        assert_eq!(v.kind, VerdictKind::NearOptimal);
    }

    #[test]
    fn no_replies_or_no_targets() {
        let lost = stats(&[None, None]);
        assert_eq!(
            judge(&Inputs {
                best: Some(&lost),
                floor_ms: None,
                first_hop: None
            })
            .kind,
            VerdictKind::Unreachable
        );
        assert_eq!(
            judge(&Inputs {
                best: None,
                floor_ms: None,
                first_hop: None
            })
            .kind,
            VerdictKind::Unknown
        );
    }
}
