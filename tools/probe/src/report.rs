//! The probe report: JSON for machines (schema in `report.schema.json`) and
//! Markdown for people.

use std::fmt::Write as _;

use serde::Serialize;

use crate::geo::Origin;
use crate::netinfo::NetworkInfo;
use crate::stats::Stats;
use crate::udp::UdpTest;
use crate::verdict::Verdict;

pub const SCHEMA_VERSION: u32 = 1;
/// The published JSON Schema for [`Report`].
pub const SCHEMA: &str = include_str!("../report.schema.json");

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub tool: Tool,
    pub generated_at: String,
    pub game: Game,
    pub options: Options,
    pub connection: Connection,
    pub regions: Vec<RegionReport>,
    pub udp_test: Option<UdpTest>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    pub name: String,
    pub version: String,
    pub os: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Game {
    pub id: String,
    pub name: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Options {
    pub count: u32,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub trace: bool,
    pub trace_probes: u32,
    pub max_hops: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct Connection {
    /// Redacted to the /24 (e.g. `203.0.113.x`).
    pub public_ip: Option<String>,
    pub network: Option<NetworkInfo>,
    pub origin: Option<Origin>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegionReport {
    pub id: String,
    pub name: String,
    pub distance_km: Option<f64>,
    pub floor_ms: Option<f64>,
    pub targets: Vec<TargetReport>,
    /// Address of the target with the lowest average latency.
    pub best_target: Option<String>,
    pub trace: Option<TraceReport>,
    pub verdict: Verdict,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Icmp,
    Tcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetSource {
    /// A relay from Valve's published relay list.
    SdrRelay,
    /// A `ping_targets` entry in the game definition.
    PingTarget,
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetReport {
    pub address: String,
    pub method: Method,
    pub source: TargetSource,
    pub label: String,
    pub stats: Option<Stats>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraceReport {
    pub target: String,
    pub reached: bool,
    pub hops: Vec<HopReport>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HopReport {
    pub ttl: u8,
    pub address: Option<String>,
    pub network: Option<NetworkInfo>,
    pub stats: Stats,
}

impl Report {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("report serialises")
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let o = &self.options;
        let _ = writeln!(out, "# PingPath probe: {}\n", self.game.name);
        let _ = writeln!(
            out,
            "{} · {} {} on {} · {} probes per target, {} ms apart{}\n",
            self.generated_at,
            self.tool.name,
            self.tool.version,
            self.tool.os,
            o.count,
            o.interval_ms,
            if o.trace {
                format!(", route traced with {} probes per hop", o.trace_probes)
            } else {
                String::new()
            }
        );

        let c = &self.connection;
        let network = c
            .network
            .as_ref()
            .map_or("unknown".to_string(), describe_network);
        let origin = c
            .origin
            .as_ref()
            .map_or("unknown (pass --from <city>)".to_string(), |o| {
                o.name.clone()
            });
        let _ = writeln!(
            out,
            "**Your connection:** {network} · public IP {} · location {origin}\n",
            c.public_ip.as_deref().unwrap_or("unknown")
        );
        for note in &c.notes {
            let _ = writeln!(out, "- {note}");
        }
        if !c.notes.is_empty() {
            out.push('\n');
        }

        for region in &self.regions {
            render_region(&mut out, region);
        }

        if let Some(udp) = &self.udp_test {
            let _ = writeln!(out, "## UDP test against {}\n", udp.endpoint);
            let _ = writeln!(out, "**Verdict:** {:?}. {}\n", udp.verdict, udp.detail);
            let _ = writeln!(out, "| Packets | Loss | Avg | Jitter |\n|---|---|---|---|");
            for (name, s) in [
                ("Plain", &udp.plain),
                ("WireGuard-shaped", &udp.wireguard_like),
            ] {
                let _ = writeln!(
                    out,
                    "| {name} | {}% | {} | {} |",
                    s.loss_pct,
                    ms(s.avg_ms),
                    ms(s.jitter_ms)
                );
            }
            out.push('\n');
        }

        if !self.notes.is_empty() {
            let _ = writeln!(out, "## Notes\n");
            for note in &self.notes {
                let _ = writeln!(out, "- {note}");
            }
            out.push('\n');
        }
        out
    }
}

fn render_region(out: &mut String, region: &RegionReport) {
    let _ = writeln!(out, "## {} ({})\n", region.name, region.id);
    let _ = writeln!(out, "**Verdict:** {}\n", region.verdict.summary);
    for reason in &region.verdict.reasons {
        let _ = writeln!(out, "- {reason}");
    }
    let distance = region.distance_km.zip(region.floor_ms);
    if let Some((km, floor)) = distance {
        let _ = writeln!(
            out,
            "- Distance {km:.0} km; the speed of light in fiber allows {floor:.1} ms at best."
        );
    }
    if !region.verdict.reasons.is_empty() || distance.is_some() {
        out.push('\n');
    }

    if !region.targets.is_empty() {
        let _ = writeln!(
            out,
            "| Target | Method | Loss | Min | Avg | P95 | Jitter |\n|---|---|---|---|---|---|---|"
        );
        for t in &region.targets {
            let target = format!("{} ({})", t.address, t.label);
            match (&t.stats, &t.error) {
                (Some(s), _) => {
                    let _ = writeln!(
                        out,
                        "| {target} | {:?} | {}% | {} | {} | {} | {} |",
                        t.method,
                        s.loss_pct,
                        ms(s.min_ms),
                        ms(s.avg_ms),
                        ms(s.p95_ms),
                        ms(s.jitter_ms)
                    );
                }
                (None, error) => {
                    let _ = writeln!(
                        out,
                        "| {target} | {:?} | error: {} | | | | |",
                        t.method,
                        error.as_deref().unwrap_or("?")
                    );
                }
            }
        }
        out.push('\n');
    }

    if let Some(trace) = &region.trace {
        match &trace.error {
            Some(error) => {
                let _ = writeln!(out, "Route to {}: not traced ({error}).\n", trace.target);
            }
            None => {
                let reached = if trace.reached {
                    ""
                } else {
                    " (target did not answer; showing the routers that did)"
                };
                let _ = writeln!(out, "Route to {}{reached}:\n", trace.target);
                let _ = writeln!(
                    out,
                    "| Hop | Address | Network | Loss | Avg | Jitter |\n|---|---|---|---|---|---|"
                );
                for hop in &trace.hops {
                    let network = hop.network.as_ref().map_or_else(
                        || {
                            if hop.address.is_some() {
                                "private".to_string()
                            } else {
                                String::new()
                            }
                        },
                        describe_network,
                    );
                    let _ = writeln!(
                        out,
                        "| {} | {} | {network} | {}% | {} | {} |",
                        hop.ttl,
                        hop.address.as_deref().unwrap_or("*"),
                        hop.stats.loss_pct,
                        ms(hop.stats.avg_ms),
                        ms(hop.stats.jitter_ms)
                    );
                }
                out.push('\n');
            }
        }
    }
    for note in &region.notes {
        let _ = writeln!(out, "- {note}");
    }
    if !region.notes.is_empty() {
        out.push('\n');
    }
}

fn describe_network(n: &NetworkInfo) -> String {
    let mut s = format!("AS{}", n.asn);
    if let Some(name) = &n.as_name {
        let _ = write!(s, " {name}");
    }
    if let Some(cc) = &n.country {
        let _ = write!(s, " ({cc})");
    }
    s
}

fn ms(value: Option<f64>) -> String {
    value.map_or("-".to_string(), |v| format!("{v:.1}"))
}
