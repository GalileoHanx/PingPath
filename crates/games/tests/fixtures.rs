//! Every fixture under tests/fixtures/invalid/ starts with an `# expect:` line
//! naming a substring of the problem it must produce. A schema-invalid game
//! file therefore fails `cargo test`, and with it CI.

use std::fs;
use std::path::{Path, PathBuf};

use pingpath_games::{definition_files, load_file};

fn fixtures(kind: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(kind);
    definition_files(&dir).expect("fixture directory exists")
}

#[test]
fn valid_fixtures_load() {
    let files = fixtures("valid");
    assert!(!files.is_empty());
    for file in files {
        if let Err(error) = load_file(&file) {
            panic!("expected a valid definition, got {error}");
        }
    }
}

#[test]
fn invalid_fixtures_are_rejected_with_the_expected_problem() {
    let files = fixtures("invalid");
    assert!(
        files.len() >= 20,
        "only {} invalid fixtures found",
        files.len()
    );
    for file in files {
        let source = fs::read_to_string(&file).unwrap();
        let expected = source
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("# expect: "))
            .unwrap_or_else(|| panic!("{} must start with `# expect: <problem>`", file.display()));

        match load_file(&file) {
            Ok(_) => panic!("{} was accepted but must be rejected", file.display()),
            Err(error) => assert!(
                error.problems.iter().any(|p| p.contains(expected)),
                "{}: expected a problem containing {expected:?}, got {:#?}",
                file.display(),
                error.problems
            ),
        }
    }
}
