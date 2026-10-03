//! The game definitions shipped in games/ must all be valid.

use std::path::Path;

use pingpath_games::load_dir;

#[test]
fn repository_game_definitions_are_valid() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../games");
    match load_dir(&dir) {
        Ok(games) => assert!(!games.is_empty(), "games/ has no definitions"),
        Err(errors) => panic!(
            "invalid game definitions:\n{}",
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}
