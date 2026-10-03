use std::time::Duration;

use serde::Serialize;

/// Summary of a series of probes; times in milliseconds, rounded to 0.01.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Stats {
    pub sent: u32,
    pub received: u32,
    pub loss_pct: f64,
    pub min_ms: Option<f64>,
    pub avg_ms: Option<f64>,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub max_ms: Option<f64>,
    /// Mean difference between consecutive replies (RFC 3550 style, without
    /// smoothing).
    pub jitter_ms: Option<f64>,
}

impl Stats {
    /// `samples` in send order; `None` is a lost probe.
    pub fn from_samples(samples: &[Option<Duration>]) -> Self {
        let rtts: Vec<f64> = samples
            .iter()
            .flatten()
            .map(|d| d.as_secs_f64() * 1000.0)
            .collect();
        let sent = samples.len() as u32;
        let received = rtts.len() as u32;
        let loss_pct = if sent == 0 {
            0.0
        } else {
            100.0 * f64::from(sent - received) / f64::from(sent)
        };

        let mut sorted = rtts.clone();
        sorted.sort_by(f64::total_cmp);
        let percentile = |p: f64| {
            let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
            sorted.get(rank.saturating_sub(1)).copied()
        };
        let jitter = (rtts.len() >= 2).then(|| {
            rtts.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (rtts.len() - 1) as f64
        });

        Self {
            sent,
            received,
            loss_pct: round(loss_pct),
            min_ms: sorted.first().copied().map(round),
            avg_ms: (!rtts.is_empty()).then(|| round(rtts.iter().sum::<f64>() / rtts.len() as f64)),
            p50_ms: percentile(50.0).map(round),
            p95_ms: percentile(95.0).map(round),
            max_ms: sorted.last().copied().map(round),
            jitter_ms: jitter.map(round),
        }
    }
}

fn round(ms: f64) -> f64 {
    (ms * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(values: &[Option<u64>]) -> Vec<Option<Duration>> {
        values
            .iter()
            .map(|v| v.map(Duration::from_millis))
            .collect()
    }

    #[test]
    fn summarises_replies_and_losses() {
        let stats = Stats::from_samples(&ms(&[Some(10), None, Some(14), Some(12), Some(20)]));
        assert_eq!(stats.sent, 5);
        assert_eq!(stats.received, 4);
        assert_eq!(stats.loss_pct, 20.0);
        assert_eq!(stats.min_ms, Some(10.0));
        assert_eq!(stats.avg_ms, Some(14.0));
        assert_eq!(stats.p50_ms, Some(12.0));
        assert_eq!(stats.p95_ms, Some(20.0));
        assert_eq!(stats.max_ms, Some(20.0));
        // |14-10| + |12-14| + |20-12| = 14, over 3 gaps
        assert_eq!(stats.jitter_ms, Some(4.67));
    }

    #[test]
    fn all_lost() {
        let stats = Stats::from_samples(&ms(&[None, None]));
        assert_eq!(stats.loss_pct, 100.0);
        assert_eq!(stats.avg_ms, None);
        assert_eq!(stats.jitter_ms, None);
    }
}
