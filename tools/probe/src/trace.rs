//! mtr-style traceroute: every TTL is probed several times in parallel, so
//! each hop gets loss, latency and jitter figures, not just one sample.

use std::collections::HashMap;
use std::io;
use std::net::Ipv4Addr;
use std::thread;
use std::time::Duration;

use crate::icmp::{Pinger, Reply};
use crate::measure::{SeriesOptions, paced};
use crate::stats::Stats;

#[derive(Debug, Clone)]
pub struct RawHop {
    pub ttl: u8,
    /// The router (or target) that answered most often at this TTL.
    pub address: Option<Ipv4Addr>,
    pub stats: Stats,
    pub reached_target: bool,
}

#[derive(Debug, Clone)]
pub struct RawTrace {
    pub target: Ipv4Addr,
    pub reached: bool,
    pub hops: Vec<RawHop>,
}

/// Probes TTL 1 to `max_hops` towards `target`, `options.count` times each.
pub fn trace(target: Ipv4Addr, max_hops: u8, options: &SeriesOptions) -> io::Result<RawTrace> {
    // Fail early, with the platform's explanation, if ICMP isn't available.
    drop(Pinger::new()?);

    let mut hops: Vec<RawHop> = thread::scope(|scope| {
        let handles: Vec<_> = (1..=max_hops)
            .map(|ttl| {
                // Stagger starts so routers don't rate-limit a burst of probes.
                let delay = Duration::from_millis(u64::from(ttl) * 7);
                scope.spawn(move || {
                    thread::sleep(delay);
                    probe_hop(target, ttl, options)
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    hops.sort_by_key(|h| h.ttl);

    let reached_at = hops.iter().position(|h| h.reached_target);
    match reached_at {
        Some(i) => hops.truncate(i + 1),
        None => {
            // Drop the run of silent TTLs after the last router that answered.
            let last = hops
                .iter()
                .rposition(|h| h.address.is_some())
                .map_or(0, |i| i + 1);
            hops.truncate(last);
        }
    }
    Ok(RawTrace {
        target,
        reached: reached_at.is_some(),
        hops,
    })
}

fn probe_hop(target: Ipv4Addr, ttl: u8, options: &SeriesOptions) -> RawHop {
    let mut pinger = match Pinger::new() {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(ttl, error = %e, "cannot probe hop");
            return RawHop {
                ttl,
                address: None,
                stats: Stats::from_samples(&[]),
                reached_target: false,
            };
        }
    };
    let mut responders: HashMap<Ipv4Addr, u32> = HashMap::new();
    let mut reached_target = false;
    let samples = paced(options, |seq| {
        match pinger.send(target, ttl, seq, options.timeout) {
            Ok(Reply::TtlExceeded { from, rtt }) => {
                *responders.entry(from).or_default() += 1;
                Some(rtt)
            }
            Ok(Reply::Echo { from, rtt }) if from == target => {
                *responders.entry(from).or_default() += 1;
                reached_target = true;
                Some(rtt)
            }
            Ok(Reply::Unreachable { from, code }) => {
                tracing::debug!(ttl, ?from, code, "unreachable");
                None
            }
            Ok(_) => None,
            Err(e) => {
                tracing::debug!(ttl, error = %e, "probe failed");
                None
            }
        }
    });
    let address = responders
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(ip, _)| ip);
    RawHop {
        ttl,
        address,
        stats: Stats::from_samples(&samples),
        reached_target,
    }
}
