//! Game definitions: the community-maintained list of games PingPath knows,
//! their server ranges, ports, regions and anti-cheat status.
//!
//! Every component reads games through this crate, so the client, the relay
//! allowlist and CI all apply the same rules.

mod model;
mod validate;

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub use model::{
    AntiCheat, GameDefinition, Location, PingTarget, PortRange, Ports, Region, RoutingPolicy,
    SupportStatus,
};
pub use validate::{
    SCHEMA, check_catalog, non_public, overlaps_with_refused, parse_definition, shares_ranges,
};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded_games.rs"));
}

/// The definitions in games/ at build time, parsed and validated.
pub fn embedded() -> Result<Vec<GameDefinition>, String> {
    embedded::EMBEDDED_GAMES
        .iter()
        .map(|(file, source)| {
            parse_definition(source).map_err(|problems| format!("{file}: {}", problems.join("; ")))
        })
        .collect()
}

/// The definitions in `dir` if given, otherwise the embedded ones.
pub fn load(dir: Option<&Path>) -> Result<Vec<GameDefinition>, String> {
    match dir {
        Some(dir) => load_dir(dir).map_err(|errors| {
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        }),
        None => embedded(),
    }
}

/// A definition file that could not be loaded, with every problem found.
#[derive(Debug)]
pub struct LoadError {
    pub path: PathBuf,
    pub problems: Vec<String>,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:", self.path.display())?;
        for problem in &self.problems {
            write!(f, "\n  - {problem}")?;
        }
        Ok(())
    }
}

impl std::error::Error for LoadError {}

/// Loads and validates one definition file.
///
/// On top of [`parse_definition`], checks that the `id` matches the file
/// name and that the `report`, if any, exists. Reports are resolved against
/// the parent of the directory holding the file (`games/` → repository root).
pub fn load_file(path: &Path) -> Result<GameDefinition, LoadError> {
    let fail = |problems| LoadError {
        path: path.to_path_buf(),
        problems,
    };

    let source = fs::read_to_string(path).map_err(|e| fail(vec![format!("cannot read: {e}")]))?;
    let definition = parse_definition(&source).map_err(fail)?;

    let mut problems = Vec::new();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if definition.id != stem {
        problems.push(format!(
            "id: \"{}\" does not match the file name; rename the file to {}.yaml",
            definition.id, definition.id
        ));
    }
    if let Some(report) = &definition.report {
        let root = fs::canonicalize(path)
            .ok()
            .and_then(|p| Some(p.parent()?.parent()?.to_path_buf()));
        if !root.is_some_and(|root| root.join(report).is_file()) {
            problems.push(format!("report: {report} does not exist"));
        }
    }

    if problems.is_empty() {
        Ok(definition)
    } else {
        Err(fail(problems))
    }
}

/// Lists the definition files (`*.yaml`, `*.yml`) directly inside `dir`,
/// sorted by path.
pub fn definition_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let is_yaml = path
            .extension()
            .is_some_and(|ext| ext == "yaml" || ext == "yml");
        if is_yaml && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Loads every definition in `dir`. Fails if any file is invalid, returning
/// the errors of all invalid files.
pub fn load_dir(dir: &Path) -> Result<Vec<GameDefinition>, Vec<LoadError>> {
    let files = definition_files(dir).map_err(|e| {
        vec![LoadError {
            path: dir.to_path_buf(),
            problems: vec![format!("cannot list: {e}")],
        }]
    })?;

    let mut definitions = Vec::new();
    let mut errors = Vec::new();
    for file in files {
        match load_file(&file) {
            Ok(definition) => definitions.push(definition),
            Err(error) => errors.push(error),
        }
    }
    if errors.is_empty() {
        Ok(definitions)
    } else {
        Err(errors)
    }
}
