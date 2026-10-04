//! The live graph's numbers (docs/decisions/009-path-selection-and-failover.md):
//! once a second, the ping, jitter and loss over a sliding window.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Jitter and loss are computed over this window.
pub const WINDOW: Duration = Duration::from_secs(10);

/// Probes sent (echoes, or direct pings) and their answers.
#[derive(Debug, Default)]
pub struct Window {
    /// (sent, round trip if answered), oldest first.
    probes: VecDeque<(Instant, Option<Duration>)>,
}

/// One point on the graph.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Figures {
    /// Average round trip over the last second.
    pub rtt_ms: Option<f64>,
    /// Mean difference between consecutive round trips over the window.
    pub jitter_ms: Option<f64>,
    pub loss_pct: f64,
}

impl Window {
    pub fn sent(&mut self, at: Instant) {
        self.probes.push_back((at, None));
    }

    /// Records the answer to the probe sent at `sent`.
    pub fn answered(&mut self, sent: Instant, at: Instant) {
        if let Some(probe) = self.probes.iter_mut().find(|(s, _)| *s == sent) {
            probe.1 = Some(at.saturating_duration_since(sent));
        }
    }

    /// The figures at `now`. Probes younger than `grace` without an answer
    /// aren't counted as lost yet.
    pub fn figures(&mut self, now: Instant, grace: Duration) -> Figures {
        while self
            .probes
            .front()
            .is_some_and(|(sent, _)| now.saturating_duration_since(*sent) > WINDOW)
        {
            self.probes.pop_front();
        }
        let settled: Vec<_> = self
            .probes
            .iter()
            .filter(|(sent, rtt)| rtt.is_some() || now.saturating_duration_since(*sent) >= grace)
            .collect();
        let ms = |d: &Duration| d.as_secs_f64() * 1000.0;
        let last_second: Vec<f64> = settled
            .iter()
            .filter(|(sent, _)| {
                now.saturating_duration_since(*sent) <= Duration::from_secs(1) + grace
            })
            .filter_map(|(_, rtt)| rtt.as_ref().map(ms))
            .collect();
        let answered: Vec<f64> = settled
            .iter()
            .filter_map(|(_, rtt)| rtt.as_ref().map(ms))
            .collect();
        let jitter = (answered.len() >= 2).then(|| {
            answered
                .windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .sum::<f64>()
                / (answered.len() - 1) as f64
        });
        let loss = if settled.is_empty() {
            0.0
        } else {
            100.0 * (settled.len() - answered.len()) as f64 / settled.len() as f64
        };
        let round = |x: f64| (x * 100.0).round() / 100.0;
        Figures {
            rtt_ms: (!last_second.is_empty())
                .then(|| round(last_second.iter().sum::<f64>() / last_second.len() as f64)),
            jitter_ms: jitter.map(round),
            loss_pct: round(loss),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_ping_jitter_and_loss() {
        let mut window = Window::default();
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        for (sent, rtt) in [(0, Some(20)), (250, Some(30)), (500, None), (750, Some(20))] {
            window.sent(at(sent));
            if let Some(rtt) = rtt {
                window.answered(at(sent), at(sent + rtt));
            }
        }
        let figures = window.figures(at(1200), Duration::from_millis(300));
        assert_eq!(figures.rtt_ms, Some(23.33));
        assert_eq!(figures.jitter_ms, Some(10.0));
        assert_eq!(figures.loss_pct, 25.0);
        // Old probes leave the window.
        let later = window.figures(at(20_000), Duration::from_millis(300));
        assert_eq!((later.rtt_ms, later.loss_pct), (None, 0.0));
    }
}
