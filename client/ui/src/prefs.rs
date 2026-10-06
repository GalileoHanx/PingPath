//! The window's own preferences, kept per user in the app's data folder
//! (`%APPDATA%\net.pingpath.client\preferences.json`): the language, and
//! the games whose "still being tested" warning the player has accepted.
//! The service's settings are elsewhere and go over IPC.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The language setting: follow Windows, or a fixed one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanguagePreference {
    #[default]
    System,
    En,
    Ar,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub language: LanguagePreference,
    /// Games whose warning was accepted, by ID: asked once per game.
    pub acknowledged: BTreeSet<String>,
}

impl Prefs {
    /// Reads the preferences; a missing or unreadable file means the
    /// defaults (and the reason is logged), never a window that won't open.
    pub fn load(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
                tracing::warn!(%error, path = %path.display(), "unreadable preferences; using the defaults");
                Self::default()
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "cannot read the preferences; using the defaults");
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        // Write then rename, so a crash never leaves half a file.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)
    }
}

/// What the window shows for the language setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Language {
    pub preference: LanguagePreference,
    /// The language in use: `en` or `ar`.
    pub language: &'static str,
    /// The Windows display language, e.g. `ar-SA`.
    pub system: Option<String>,
}

impl Language {
    pub fn new(preference: LanguagePreference, system: Option<String>) -> Self {
        Self {
            preference,
            language: resolve(preference, system.as_deref()),
            system,
        }
    }
}

/// The Windows display language (the first preferred UI language).
pub fn system_locale() -> Option<String> {
    sys_locale::get_locale()
}

/// Arabic when chosen, or when following a system whose display language
/// is Arabic (any region); English otherwise.
pub fn resolve(preference: LanguagePreference, system: Option<&str>) -> &'static str {
    match preference {
        LanguagePreference::En => "en",
        LanguagePreference::Ar => "ar",
        LanguagePreference::System => match system {
            Some(locale) if locale.to_ascii_lowercase().starts_with("ar") => "ar",
            _ => "en",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_an_arabic_display_language() {
        use LanguagePreference::*;
        assert_eq!(resolve(System, Some("ar-SA")), "ar");
        assert_eq!(resolve(System, Some("AR")), "ar");
        assert_eq!(resolve(System, Some("en-US")), "en");
        assert_eq!(resolve(System, Some("fr-FR")), "en");
        assert_eq!(resolve(System, None), "en");
        assert_eq!(resolve(Ar, Some("en-US")), "ar");
        assert_eq!(resolve(En, Some("ar-AE")), "en");
    }

    #[test]
    fn preferences_round_trip_and_survive_a_bad_file() {
        let dir = std::env::temp_dir().join(format!("pingpath-ui-prefs-{}", std::process::id()));
        let path = dir.join("preferences.json");
        let mut prefs = Prefs::load(&path);
        assert_eq!(prefs, Prefs::default());
        prefs.language = LanguagePreference::Ar;
        prefs.acknowledged.insert("echo-test".into());
        prefs.save(&path).unwrap();
        assert_eq!(Prefs::load(&path), prefs);
        fs::write(&path, "{not json").unwrap();
        assert_eq!(Prefs::load(&path), Prefs::default());
        let _ = fs::remove_dir_all(dir);
    }
}
