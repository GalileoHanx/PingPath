//! Whether a game may be routed at all (PLAN.md rule 7). The UI, the
//! service, the control plane and the exit each check it; this is the
//! client's copy (docs/decisions/008-split-tunnel-routing.md).

use pingpath_games::{GameDefinition, RoutingPolicy, SupportStatus};
use serde::Serialize;

/// Why a game or region can't be optimized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Refusal {
    /// `untested` or `unsupported`: never routed.
    #[error("{game} is {status} and is never routed")]
    NotRoutable { game: String, status: String },
    #[error("{game} has no region \"{region}\"")]
    UnknownRegion { game: String, region: String },
    #[error("no game \"{game}\"")]
    UnknownGame { game: String },
    /// Its ranges overlap a game that is never routed, whose traffic would
    /// be carried too (rule 7).
    #[error("{game} shares server addresses with a game that is never routed")]
    SharesRangesWithRefusedGame { game: String },
}

/// What the client may do with a game: `Ok(Allow)`, `Ok(Warn)` for games
/// still being tested (routed with a warning), or why not.
pub fn check(game: &GameDefinition, region: &str) -> Result<RoutingPolicy, Refusal> {
    let policy = game.status.routing_policy();
    if policy == RoutingPolicy::Refuse {
        return Err(Refusal::NotRoutable {
            game: game.name.clone(),
            status: game.status.as_str().to_string(),
        });
    }
    if game.region(region).is_none() {
        return Err(Refusal::UnknownRegion {
            game: game.name.clone(),
            region: region.to_string(),
        });
    }
    Ok(policy)
}

/// Whether the UI should offer Optimize for this game at all.
pub fn offered(status: SupportStatus) -> bool {
    status.routing_policy() != RoutingPolicy::Refuse
}
