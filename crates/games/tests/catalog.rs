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

#[test]
fn repository_catalog_has_no_routable_game_sharing_ranges_with_a_refused_one() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../games");
    let games = load_dir(&dir).expect("valid definitions");
    let problems = pingpath_games::check_catalog(&games);
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn a_routable_game_may_not_share_ranges_with_a_refused_one() {
    let source = |id: &str, status: &str, range: &str| {
        format!(
            "id: {id}\nname: {id}\nstatus: {status}\nanti_cheat: [none]\nports:\n  udp: [7777]\nregions:\n  - id: eu-central\n    name: Frankfurt\n    server_ranges: [\"{range}\"]\n"
        )
    };
    let routable = pingpath_games::parse_definition(&source("a", "testing", "1.1.1.0/24")).unwrap();
    let refused = pingpath_games::parse_definition(&source("b", "untested", "1.1.1.0/25")).unwrap();
    let elsewhere =
        pingpath_games::parse_definition(&source("c", "untested", "1.0.0.0/24")).unwrap();
    let problems =
        pingpath_games::check_catalog(&[routable.clone(), refused.clone(), elsewhere.clone()]);
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(
        problems[0].contains("overlaps 1.1.1.0/25 of b (untested)"),
        "{}",
        problems[0]
    );
    assert!(pingpath_games::check_catalog(&[routable, elsewhere]).is_empty());
}
