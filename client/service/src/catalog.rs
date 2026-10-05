//! The games the service knows: the definitions built into the binary,
//! plus extra ones an administrator put in `games_dir` (a tester's echo
//! target, a game being tested). Extra definitions can only add games: one
//! with the ID of a built-in game is refused, so a file can't quietly
//! replace a game's server ranges.

use std::collections::HashSet;
use std::path::Path;

use pingpath_client_core::ipc::{GameInfo, RegionInfo};
use pingpath_client_core::policy;
use pingpath_games::{GameDefinition, SupportStatus};

/// The games, and which came from `games_dir`.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub games: Vec<GameDefinition>,
    pub extra: HashSet<String>,
}

impl Catalog {
    /// The built-in definitions plus the valid files in `extra_dir`.
    /// Problems with extra files are logged and the files skipped; the
    /// built-in definitions failing is a build error.
    pub fn load(extra_dir: Option<&Path>) -> Result<Self, String> {
        let mut games = pingpath_games::embedded()?;
        let mut extra = HashSet::new();
        if let Some(dir) = extra_dir {
            let files = pingpath_games::definition_files(dir)
                .map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
            for file in files {
                match pingpath_games::load_file(&file) {
                    Ok(game) if games.iter().any(|g| g.id == game.id) => {
                        tracing::error!(file = %file.display(), game = %game.id, "extra game definition skipped: a game with this id is built in");
                    }
                    Ok(game) => {
                        tracing::info!(file = %file.display(), game = %game.id, status = game.status.as_str(), "extra game definition loaded");
                        extra.insert(game.id.clone());
                        games.push(game);
                    }
                    Err(error) => {
                        tracing::error!(%error, "extra game definition skipped");
                    }
                }
            }
        }
        for problem in pingpath_games::check_catalog(&games) {
            // The engine refuses the games concerned; say why here.
            tracing::warn!(problem, "game catalog problem");
        }
        Ok(Self { games, extra })
    }

    /// The games as the UI lists them.
    pub fn infos(&self) -> Vec<GameInfo> {
        let mut infos: Vec<_> = self
            .games
            .iter()
            .map(|game| GameInfo {
                id: game.id.clone(),
                name: game.name.clone(),
                status: game.status.as_str().to_string(),
                routable: policy::offered(game.status)
                    && pingpath_games::overlaps_with_refused(game, &self.games).is_empty(),
                needs_warning: game.status == SupportStatus::Testing,
                regions: game
                    .regions
                    .iter()
                    .map(|r| RegionInfo {
                        id: r.id.clone(),
                        name: r.name.clone(),
                    })
                    .collect(),
                test_definition: self.extra.contains(&game.id),
            })
            .collect();
        infos.sort_by_key(|g| g.name.to_lowercase());
        infos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, id: &str, status: &str, range: &str) {
        let text = format!(
            "id: {id}
name: {id}
status: {status}
anti_cheat: [other]
ports:
  udp: [27015]
regions:
  - id: here
    name: Here
    server_ranges: [\"{range}\"]
    ping_targets: [\"9.9.9.9\"]
"
        );
        std::fs::write(dir.join(format!("{id}.yaml")), text).unwrap();
    }

    #[test]
    fn extra_definitions_add_games_but_never_replace_built_in_ones() {
        let dir = tempfile::tempdir().unwrap();
        let built_in = pingpath_games::embedded().unwrap();
        let taken = &built_in[0].id;
        write(dir.path(), taken, "untested", "1.2.3.0/24");
        write(dir.path(), "tester-echo", "testing", "1.2.5.0/24");
        std::fs::write(dir.path().join("broken.yaml"), "id: [").unwrap();

        let catalog = Catalog::load(Some(dir.path())).unwrap();
        assert_eq!(catalog.games.len(), built_in.len() + 1);
        let ranges = |games: &[GameDefinition]| {
            let game = games.iter().find(|g| &g.id == taken).unwrap();
            game.regions[0].server_ranges.clone()
        };
        assert_eq!(
            ranges(&catalog.games),
            ranges(&built_in),
            "the built-in definition is kept"
        );
        let infos = catalog.infos();
        let echo = infos.iter().find(|g| g.id == "tester-echo").unwrap();
        assert!(echo.test_definition && echo.routable && echo.needs_warning);
        assert!(
            infos
                .iter()
                .filter(|g| g.id != "tester-echo")
                .all(|g| !g.test_definition)
        );
    }

    #[test]
    fn games_that_are_never_routed_are_listed_as_such() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "refused-game", "untested", "1.2.4.0/24");
        // Shares a range with a game that is never routed (rule 7).
        write(dir.path(), "neighbour", "testing", "1.2.4.128/25");
        let infos = Catalog::load(Some(dir.path())).unwrap().infos();
        let routable = |id: &str| infos.iter().find(|g| g.id == id).unwrap().routable;
        assert!(!routable("refused-game"));
        assert!(!routable("neighbour"));
    }
}
