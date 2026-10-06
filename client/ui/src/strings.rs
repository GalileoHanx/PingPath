//! The few strings the Rust side shows itself (the tray menu and Windows
//! notifications), from the same translation files as the page, built into
//! the binary. `tools/check-i18n.py` checks the keys used here with `tr`.

use serde_json::{Map, Value};

const EN: &str = include_str!("../frontend/i18n/en.json");
const AR: &str = include_str!("../frontend/i18n/ar.json");

pub struct Strings {
    strings: Map<String, Value>,
    english: Map<String, Value>,
}

fn parse(text: &str) -> Map<String, Value> {
    match serde_json::from_str(text) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

impl Strings {
    /// The strings for `language` (`en` or `ar`), falling back to English
    /// for anything missing.
    pub fn new(language: &str) -> Self {
        let english = parse(EN);
        let strings = if language == "ar" {
            parse(AR)
        } else {
            english.clone()
        };
        Self { strings, english }
    }

    /// The string for `key`; the key itself if no file has it (the check
    /// script makes sure that never ships).
    pub fn tr(&self, key: &str) -> String {
        self.strings
            .get(key)
            .or_else(|| self.english.get(key))
            .and_then(Value::as_str)
            .unwrap_or(key)
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_languages_have_the_tray_strings() {
        for language in ["en", "ar"] {
            let strings = Strings::new(language);
            for key in [
                "tray.open",
                "tray.disable",
                "tray.quit",
                "notify.fell_back.title",
            ] {
                let text = strings.tr(key);
                assert!(!text.is_empty() && text != key, "{language}: {key}");
            }
        }
        assert_ne!(
            Strings::new("ar").tr("tray.quit"),
            Strings::new("en").tr("tray.quit")
        );
        assert_eq!(Strings::new("en").tr("no.such.key"), "no.such.key");
    }
}
