//! Where the service keeps its files (docs/decisions/007-client-architecture.md).
//!
//! On Windows everything lives under `%ProgramData%\PingPath`: the tree is
//! writable only by SYSTEM and Administrators, because the service runs as
//! SYSTEM and trusts what it reads there, and `logs\` is also readable by
//! Users so the unprivileged UI can build the diagnostic report.

use std::env;
use std::path::{Path, PathBuf};

/// The service's files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// The root: `%ProgramData%\PingPath`, or `/var/lib/pingpath` on Linux.
    pub data: PathBuf,
}

impl Paths {
    /// The system-wide location.
    pub fn system() -> Self {
        Self {
            data: system_data_dir(),
        }
    }

    /// A different root (development, tests).
    pub fn under(data: impl Into<PathBuf>) -> Self {
        Self { data: data.into() }
    }

    /// The settings (admin-only).
    pub fn config(&self) -> PathBuf {
        self.data.join("client.toml")
    }

    /// Runtime state (admin-only).
    pub fn state(&self) -> PathBuf {
        self.data.join("state")
    }

    /// The record of relay host routes the service added, for cleanup
    /// after a crash (docs/decisions/008-split-tunnel-routing.md).
    pub fn route_ledger(&self) -> PathBuf {
        self.state().join("routes.json")
    }

    /// The service's logs (readable by Users). `PINGPATH_LOG_DIR` still
    /// wins, so a developer can redirect them.
    pub fn logs(&self) -> PathBuf {
        env::var_os(pingpath_diagnostics::LOG_DIR_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.data.join("logs"))
    }

    /// Resolves the `games_dir` setting: relative paths are under the data
    /// folder, and the result must stay inside it, so extra definitions
    /// can only come from a folder only administrators can write.
    pub fn games_dir(&self, setting: &Path) -> Result<PathBuf, String> {
        let dir = if setting.is_absolute() {
            setting.to_path_buf()
        } else {
            self.data.join(setting)
        };
        let resolved = dir
            .canonicalize()
            .map_err(|e| format!("games_dir {}: {e}", dir.display()))?;
        let root = self
            .data
            .canonicalize()
            .map_err(|e| format!("{}: {e}", self.data.display()))?;
        if resolved.starts_with(&root) {
            Ok(resolved)
        } else {
            Err(format!(
                "games_dir {} is outside {}; extra game definitions must be in a folder only administrators can change",
                dir.display(),
                self.data.display()
            ))
        }
    }
}

fn system_data_dir() -> PathBuf {
    if cfg!(windows) {
        env::var_os("ProgramData")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("PingPath")
    } else {
        PathBuf::from("/var/lib/pingpath")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_live_under_the_data_folder() {
        let paths = Paths::under("/data/PingPath");
        assert_eq!(paths.config(), Path::new("/data/PingPath/client.toml"));
        assert_eq!(
            paths.route_ledger(),
            Path::new("/data/PingPath/state/routes.json")
        );
    }

    #[test]
    fn extra_games_must_stay_inside_the_data_folder() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("PingPath");
        std::fs::create_dir_all(data.join("games")).unwrap();
        let outside = root.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        let paths = Paths::under(&data);

        let inside = paths.games_dir(Path::new("games")).unwrap();
        assert!(inside.ends_with("games"));
        assert!(paths.games_dir(&outside).is_err());
        // A relative path can't climb out either.
        assert!(paths.games_dir(Path::new("../elsewhere")).is_err());
        assert!(paths.games_dir(Path::new("missing")).is_err());
    }
}
