//! The user's settings in `$XDG_CONFIG_HOME/typewhisper/settings.json`.
//!
//! Keys use the names of the macOS app's settings, so a later sync can map them
//! one to one. Keys this version does not know are kept when the file is written.

use std::io::Write;
use std::path::PathBuf;

use serde_json::{Map, Value};

/// Restore the clipboard after inserting text. Off by default, as on macOS.
pub const PRESERVE_CLIPBOARD: &str = "preserveClipboard";
/// Fit dictations to the text around the cursor. On by default, as on macOS.
pub const APP_FORMATTING: &str = "appFormattingEnabled";
/// Drop a model-added period after a value dictated into an empty field. On by
/// default, as on macOS.
pub const STRIP_STANDALONE_VALUE_PERIOD: &str = "stripFinalPeriodFromStandaloneValuesEnabled";
/// App IDs of terminals the user added to the built-in ones. The key is the
/// macOS one, where the list holds bundle identifiers.
pub const TERMINAL_APPS: &str = "terminalAppBundleIdentifiers";

pub struct Settings {
    path: PathBuf,
    values: Map<String, Value>,
}

impl Settings {
    /// Reads the settings file. A missing or unreadable file gives the defaults.
    pub fn load() -> Self {
        let path = crate::config_dir()
            .map(|dir| dir.join("settings.json"))
            .unwrap_or_else(|_| PathBuf::from("settings.json"));
        Self::load_from(path)
    }

    pub fn load_from(path: PathBuf) -> Self {
        let values = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Map<String, Value>>(&text).ok())
            .unwrap_or_default();
        Self { path, values }
    }

    pub fn bool(&self, key: &str, default: bool) -> bool {
        self.values
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(default)
    }

    /// A list of strings. Entries that are not strings are skipped.
    pub fn strings(&self, key: &str) -> Vec<String> {
        self.values
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Changes one value and writes the file right away.
    pub fn set_bool(&mut self, key: &str, value: bool) -> std::io::Result<()> {
        self.values.insert(key.to_owned(), Value::Bool(value));
        self.save()
    }

    /// Changes a list of strings and writes the file right away.
    pub fn set_strings(&mut self, key: &str, values: &[String]) -> std::io::Result<()> {
        let list = values.iter().cloned().map(Value::String).collect();
        self.values.insert(key.to_owned(), Value::Array(list));
        self.save()
    }

    /// Writes to a temporary file first, so a crash cannot leave half a file.
    fn save(&self) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(&self.values)?;
        let temporary = self.path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_unknown_keys_and_defaults_missing_ones() {
        let dir = std::env::temp_dir().join(format!("tw-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"someMacKey": 3}"#).unwrap();

        let mut settings = Settings::load_from(path.clone());
        assert!(!settings.bool(PRESERVE_CLIPBOARD, false));
        settings.set_bool(PRESERVE_CLIPBOARD, true).unwrap();

        let again = Settings::load_from(path.clone());
        assert!(again.bool(PRESERVE_CLIPBOARD, false));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("someMacKey"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keeps_a_list_of_strings() {
        let dir = std::env::temp_dir().join(format!("tw-settings-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"terminalAppBundleIdentifiers": ["a", 3, "b"]}"#).unwrap();

        let mut settings = Settings::load_from(path.clone());
        assert_eq!(settings.strings(TERMINAL_APPS), ["a", "b"]);
        assert!(settings.strings("missing").is_empty());
        settings
            .set_strings(TERMINAL_APPS, &["org.example.Term".to_owned()])
            .unwrap();
        let again = Settings::load_from(path);
        assert_eq!(again.strings(TERMINAL_APPS), ["org.example.Term"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn broken_file_gives_defaults() {
        let dir = std::env::temp_dir().join(format!("tw-settings-broken-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(Settings::load_from(path).bool(PRESERVE_CLIPBOARD, true));
        let _ = std::fs::remove_dir_all(dir);
    }
}
