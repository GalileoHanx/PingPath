//! Choosing a path (docs/decisions/009-path-selection-and-failover.md):
//! estimate every way to reach the game region through PingPath, compare
//! the best with the direct route, and keep the others with the same exit
//! as standbys. Pure functions over measurements, so the choice can be
//! tested exhaustively.

use std::collections::HashMap;

use serde::Serialize;

use crate::control::{Latency, Relay};

/// A candidate with more loss than this on a leg is not used, if at least
/// [`MIN_LOST`] probes were lost: one lost probe is noise, and in a short
/// series it would read as 10 to 33%.
pub const MAX_LOSS_PCT: f64 = 2.0;
pub const MIN_LOST: f64 = 2.0;
/// An exit further than this from the game region isn't in that region
/// (rule 3: the exit sits in the game server's region), so it isn't used.
pub const MAX_EXIT_TO_REGION_MS: f64 = 30.0;

/// One measured leg.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Leg {
    pub rtt_ms: f64,
    pub loss_pct: f64,
    /// Probes behind `loss_pct`; 0 when not known.
    pub samples: u32,
}

impl Leg {
    /// Whether the leg's loss allows using it.
    pub fn usable(&self) -> bool {
        let lost = (self.loss_pct / 100.0 * f64::from(self.samples)).round();
        self.loss_pct <= MAX_LOSS_PCT || (self.samples > 0 && lost < MIN_LOST)
    }
}

/// A way to reach the game region: straight to an exit, or through an entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub exit: u32,
    pub entry: Option<u32>,
    /// player → (entry →) exit → region, in milliseconds.
    pub estimate_ms: f64,
    /// The part the player's own measurements and the relays' cover
    /// before the region: player → (entry →) exit.
    pub to_exit_ms: f64,
    pub exit_to_region_ms: f64,
}

/// Everything the choice needs.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub relays: Vec<Relay>,
    /// Player → relay, by relay ID (PROBE frames).
    pub player_to_relay: HashMap<u32, Leg>,
    /// From the control plane.
    pub latency: Latency,
}

impl Inputs {
    /// Entry → exit. A relay probes exactly the peers in its own config
    /// and forwards only to those, so the entry's own row for the exit is
    /// what shows it can carry traffic there; the exit's row for the
    /// entry, if any, may give the faster figure.
    fn between(&self, entry: u32, exit: u32) -> Option<Leg> {
        let leg = |row: &crate::control::RelayLatency| {
            row.rtt_ms.map(|rtt_ms| Leg {
                rtt_ms,
                loss_pct: row.loss_pct,
                samples: row.samples,
            })
        };
        let rows = &self.latency.latency;
        let own = rows
            .iter()
            .find(|row| row.relay_id == entry && row.peer_id == exit)?;
        std::iter::once(own)
            .chain(
                rows.iter()
                    .filter(|row| row.relay_id == exit && row.peer_id == entry),
            )
            .filter_map(leg)
            .min_by(|x, y| x.rtt_ms.total_cmp(&y.rtt_ms))
    }

    fn exit_to_region(&self, exit: u32, game: &str, region: &str) -> Option<Leg> {
        self.latency
            .regions
            .iter()
            .find(|row| row.relay_id == exit && row.game == game && row.region == region)
            .and_then(|row| {
                row.rtt_ms.map(|rtt_ms| Leg {
                    rtt_ms,
                    loss_pct: row.loss_pct,
                    samples: row.samples,
                })
            })
    }
}

/// Every usable candidate for `game` in `region`, best first. Ties go to
/// fewer hops.
pub fn candidates(game: &str, region: &str, inputs: &Inputs) -> Vec<Candidate> {
    let mut found = Vec::new();
    for exit in inputs
        .relays
        .iter()
        .filter(|r| r.serves(region) && r.public_key.is_some())
    {
        let to_region = match judge(inputs.exit_to_region(exit.id, game, region)) {
            Ok(leg) => leg,
            Err(why) => {
                tracing::info!(exit = %exit.name, %game, %region, why = %why, "exit not used: its distance to the game region");
                continue;
            }
        };
        if to_region.rtt_ms > MAX_EXIT_TO_REGION_MS {
            tracing::info!(exit = %exit.name, %game, %region, rtt_ms = to_region.rtt_ms, "exit is too far from the game region to use (rule 3)");
            continue;
        }
        match judge(inputs.player_to_relay.get(&exit.id).copied()) {
            Ok(direct) => found.push(Candidate {
                exit: exit.id,
                entry: None,
                estimate_ms: direct.rtt_ms + to_region.rtt_ms,
                to_exit_ms: direct.rtt_ms,
                exit_to_region_ms: to_region.rtt_ms,
            }),
            Err(why) => {
                tracing::info!(exit = %exit.name, why = %why, "path straight to the exit not used: the player's leg");
            }
        }
        for entry in inputs
            .relays
            .iter()
            .filter(|r| r.is_entry() && r.id != exit.id)
        {
            let first = judge(inputs.player_to_relay.get(&entry.id).copied());
            let middle = judge(inputs.between(entry.id, exit.id));
            let (first, middle) = match (first, middle) {
                (Ok(first), Ok(middle)) => (first, middle),
                (Err(why), _) => {
                    tracing::info!(exit = %exit.name, entry = %entry.name, why = %why, "path not used: the player's leg to the entry");
                    continue;
                }
                (_, Err(why)) => {
                    tracing::info!(exit = %exit.name, entry = %entry.name, why = %why, "path not used: the entry's leg to the exit (an entry forwards only to exits it measures)");
                    continue;
                }
            };
            found.push(Candidate {
                exit: exit.id,
                entry: Some(entry.id),
                estimate_ms: first.rtt_ms + middle.rtt_ms + to_region.rtt_ms,
                to_exit_ms: first.rtt_ms + middle.rtt_ms,
                exit_to_region_ms: to_region.rtt_ms,
            });
        }
    }
    found.sort_by(|a, b| {
        a.estimate_ms
            .total_cmp(&b.estimate_ms)
            .then(a.entry.is_some().cmp(&b.entry.is_some()))
    });
    found
}

/// A leg the choice can use, or why not, for the log (PLAN.md section 9:
/// each path choice with the measurements behind it).
fn judge(leg: Option<Leg>) -> Result<Leg, String> {
    match leg {
        None => Err("not measured".into()),
        Some(leg) if !leg.usable() => Err(format!(
            "lossy: {}% of {} probes",
            leg.loss_pct, leg.samples
        )),
        Some(leg) => Ok(leg),
    }
}

/// The other candidates with the same exit as `chosen`, best first: where
/// traffic moves if the chosen path fails, without the game server seeing
/// a new address.
pub fn standbys(chosen: &Candidate, all: &[Candidate]) -> Vec<Candidate> {
    all.iter()
        .filter(|c| c.exit == chosen.exit && c.entry != chosen.entry)
        .cloned()
        .collect()
}

/// Whether `optimized_ms` beats `direct_ms` by enough to switch: at least
/// `min_ms`, or at least `min_pct` percent of the direct ping
/// (PLAN.md Phase 3: "≥10% or ≥10 ms").
pub fn worthwhile(direct_ms: f64, optimized_ms: f64, min_ms: f64, min_pct: f64) -> bool {
    let improvement = direct_ms - optimized_ms;
    improvement > 0.0 && (improvement >= min_ms || improvement >= direct_ms * min_pct / 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{RegionLatency, RelayLatency};

    fn relay(id: u32, roles: &[&str], regions: &[&str]) -> Relay {
        Relay {
            id,
            name: format!("r{id}"),
            address: format!("192.0.2.{id}:51820").parse().unwrap(),
            roles: roles.iter().map(|r| r.to_string()).collect(),
            regions: regions.iter().map(|r| r.to_string()).collect(),
            public_key: roles.contains(&"exit").then(|| "key".into()),
            provider: None,
            location: None,
        }
    }

    fn leg(rtt_ms: f64) -> Leg {
        Leg {
            rtt_ms,
            loss_pct: 0.0,
            samples: 10,
        }
    }

    fn inputs() -> Inputs {
        // 1: exit in eu-central; 2: entry near the player; 3: another entry; 4: exit elsewhere.
        let relays = vec![
            relay(1, &["exit"], &["eu-central"]),
            relay(2, &["entry"], &[]),
            relay(3, &["entry"], &[]),
            relay(4, &["exit"], &["us-east"]),
        ];
        let player_to_relay = HashMap::from([
            (1, leg(90.0)),
            (2, leg(5.0)),
            (3, leg(12.0)),
            (4, leg(150.0)),
        ]);
        let row = |relay_id, peer_id, rtt| RelayLatency {
            relay_id,
            peer_id,
            rtt_ms: Some(rtt),
            loss_pct: 0.0,
            samples: 30,
            age_secs: 1,
        };
        let latency = Latency {
            latency: vec![row(2, 1, 60.0), row(1, 3, 55.0), row(3, 1, 58.0)],
            regions: vec![RegionLatency {
                game: "cs2".into(),
                relay_id: 1,
                region: "eu-central".into(),
                rtt_ms: Some(2.0),
                loss_pct: 0.0,
                samples: 18,
                age_secs: 1,
            }],
        };
        Inputs {
            relays,
            player_to_relay,
            latency,
        }
    }

    #[test]
    fn estimates_every_path_and_ranks_them() {
        let found = candidates("cs2", "eu-central", &inputs());
        let summary: Vec<(Option<u32>, f64)> =
            found.iter().map(|c| (c.entry, c.estimate_ms)).collect();
        // 5 + 60 + 2; 12 + 55 (the faster direction) + 2; straight 90 + 2.
        assert_eq!(
            summary,
            vec![(Some(2), 67.0), (Some(3), 69.0), (None, 92.0)]
        );
        assert!(
            found.iter().all(|c| c.exit == 1),
            "only exits serving the region"
        );
        assert_eq!(found[0].to_exit_ms, 65.0);
        let standbys = standbys(&found[0], &found);
        assert_eq!(
            standbys.iter().map(|c| c.entry).collect::<Vec<_>>(),
            vec![Some(3), None]
        );
    }

    #[test]
    fn skips_lossy_or_unmeasured_legs() {
        let mut inputs = inputs();
        inputs.player_to_relay.insert(
            2,
            Leg {
                rtt_ms: 5.0,
                loss_pct: 20.0,
                samples: 10,
            },
        );
        inputs
            .latency
            .latency
            .retain(|r| r.relay_id != 3 && r.peer_id != 3);
        let found = candidates("cs2", "eu-central", &inputs);
        assert_eq!(
            found.iter().map(|c| c.entry).collect::<Vec<_>>(),
            vec![None]
        );
        // Without the exit's region figure nothing can be estimated.
        inputs.latency.regions.clear();
        assert!(candidates("cs2", "eu-central", &inputs).is_empty());
    }

    #[test]
    fn one_lost_probe_is_noise_two_are_loss() {
        let leg = |loss_pct, samples| Leg {
            rtt_ms: 10.0,
            loss_pct,
            samples,
        };
        assert!(leg(0.0, 10).usable());
        assert!(leg(10.0, 10).usable(), "1 of 10");
        assert!(!leg(20.0, 10).usable(), "2 of 10");
        assert!(leg(3.3, 30).usable(), "1 of 30");
        assert!(!leg(6.7, 30).usable(), "2 of 30");
        assert!(leg(2.0, 100).usable(), "2 of 100 is within 2%");
        assert!(!leg(3.0, 100).usable());
        // Without a count, the percentage alone decides.
        assert!(!leg(10.0, 0).usable());
        assert!(leg(2.0, 0).usable());
    }

    #[test]
    fn an_entry_is_used_only_for_exits_it_reports() {
        // Relay 3 doesn't list exit 1 among its peers, so it never measures
        // it and can't forward to it; the exit's own row for it proves
        // nothing.
        let mut inputs = inputs();
        inputs
            .latency
            .latency
            .retain(|r| !(r.relay_id == 3 && r.peer_id == 1));
        let found = candidates("cs2", "eu-central", &inputs);
        assert_eq!(
            found.iter().map(|c| c.entry).collect::<Vec<_>>(),
            vec![Some(2), None]
        );
    }

    #[test]
    fn uses_only_the_games_own_region_figure_from_an_exit_in_the_region() {
        // Another game's figure for a region with the same ID doesn't count.
        let mut inputs = inputs();
        inputs.latency.regions[0].game = "dota-2".into();
        assert!(candidates("cs2", "eu-central", &inputs).is_empty());
        // An exit far from the region isn't in it (rule 3).
        let mut inputs = super::tests::inputs();
        inputs.latency.regions[0].rtt_ms = Some(MAX_EXIT_TO_REGION_MS + 5.0);
        assert!(candidates("cs2", "eu-central", &inputs).is_empty());
    }

    #[test]
    fn switches_only_when_clearly_better() {
        // 10 ms or 10%, whichever comes first.
        assert!(worthwhile(120.0, 109.0, 10.0, 10.0)); // 11 ms
        assert!(!worthwhile(120.0, 111.0, 10.0, 10.0)); // 9 ms, 7.5%
        assert!(worthwhile(50.0, 44.0, 10.0, 10.0)); // 6 ms but 12%
        assert!(!worthwhile(50.0, 50.0, 0.0, 0.0)); // never when not faster
        assert!(!worthwhile(50.0, 60.0, 10.0, 10.0));
    }
}
