//! `pingpath-games validate <PATH>...`: checks game definition files.
//! A directory path checks every `*.yaml` / `*.yml` file directly inside it.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use pingpath_games::{definition_files, load_file};

const USAGE: &str = "\
Usage: pingpath-games validate <PATH>...

Validates PingPath game definitions against games/schema.json and the
rules in PLAN.md. A directory checks every *.yaml / *.yml file in it.
Exits with status 1 if any file is invalid.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.split_first() {
        Some((command, paths)) if command == "validate" && !paths.is_empty() => validate(paths),
        Some((flag, _)) if flag == "-h" || flag == "--help" => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn validate(paths: &[String]) -> ExitCode {
    let mut files: Vec<PathBuf> = Vec::new();
    for arg in paths {
        let path = Path::new(arg);
        if path.is_dir() {
            match definition_files(path) {
                Ok(found) => files.extend(found),
                Err(e) => {
                    eprintln!("error: cannot list {}: {e}", path.display());
                    return ExitCode::FAILURE;
                }
            }
        } else {
            files.push(path.to_path_buf());
        }
    }

    let mut failed = 0;
    for file in &files {
        match load_file(file) {
            Ok(game) => println!("ok    {} ({}, {})", file.display(), game.name, game.status),
            Err(error) => {
                failed += 1;
                println!("FAIL  {error}");
            }
        }
    }

    println!("{} file(s) checked, {failed} invalid", files.len());
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
