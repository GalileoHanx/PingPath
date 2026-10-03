//! The games the probe knows: embedded at build time, or read from a
//! directory with `--games-dir` (for trying out a new definition).

use std::path::Path;

use pingpath_games::{GameDefinition, load_dir, parse_definition};

include!(concat!(env!("OUT_DIR"), "/embedded_games.rs"));

pub fn load(dir: Option<&Path>) -> Result<Vec<GameDefinition>, String> {
    match dir {
        Some(dir) => load_dir(dir).map_err(|errors| {
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        }),
        None => EMBEDDED_GAMES
            .iter()
            .map(|(file, source)| {
                parse_definition(source)
                    .map_err(|problems| format!("{file}: {}", problems.join("; ")))
            })
            .collect(),
    }
}

pub fn find<'a>(games: &'a [GameDefinition], id: &str) -> Result<&'a GameDefinition, String> {
    games.iter().find(|g| g.id == id).ok_or_else(|| {
        let ids: Vec<&str> = games.iter().map(|g| g.id.as_str()).collect();
        format!("unknown game \"{id}\"; known games: {}", ids.join(", "))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_games_load() {
        let games = load(None).unwrap();
        assert!(find(&games, "counter-strike-2").is_ok());
        assert!(
            find(&games, "nope")
                .unwrap_err()
                .contains("counter-strike-2")
        );
    }
}
