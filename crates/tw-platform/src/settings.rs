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
/// Plugin ID of the engine that transcribes dictations, such as
/// `com.typewhisper.whisper-cpp`. Missing means NVIDIA Parakeet. The macOS key,
/// where it holds the engine's provider ID.
pub const SELECTED_ENGINE: &str = "selectedEngine";
/// The model picked for an engine with a choice of models, such as `base` for
/// Whisper. It stays while an engine with a single model is in use, so
/// switching back finds the same model. The macOS key.
pub const SELECTED_MODEL: &str = "selectedModelId";

/// The key of a plugin's setting, `plugin.<plugin ID>.<name>`, as the macOS
/// app saves the settings of its plugins.
pub fn plugin_key(plugin_id: &str, name: &str) -> String {
    format!("plugin.{plugin_id}.{name}")
}

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

    pub fn string(&self, key: &str) -> Option<String> {
        self.values
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
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

    /// The settings of one plugin by name, as the plugin gets them. Names with
    /// a dot are left out: they belong to a plugin whose ID starts with this one.
    pub fn plugin_values(&self, plugin_id: &str) -> Map<String, Value> {
        let prefix = plugin_key(plugin_id, "");
        self.values
            .iter()
            .filter_map(|(key, value)| {
                let name = key.strip_prefix(&prefix)?;
                (!name.is_empty() && !name.contains('.')).then(|| (name.to_owned(), value.clone()))
            })
            .collect()
    }

    /// Changes one setting of a plugin and writes the file right away. `Null`
    /// removes it.
    pub fn set_plugin_value(
        &mut self,
        plugin_id: &str,
        name: &str,
        value: Value,
    ) -> std::io::Result<()> {
        let key = plugin_key(plugin_id, name);
        if value.is_null() {
            self.values.remove(&key);
        } else {
            self.values.insert(key, value);
        }
        self.save()
    }

    /// Changes one value and writes the file right away.
    pub fn set_bool(&mut self, key: &str, value: bool) -> std::io::Result<()> {
        self.values.insert(key.to_owned(), Value::Bool(value));
        self.save()
    }

    /// Changes one text and writes the file right away.
    pub fn set_string(&mut self, key: &str, value: &str) -> std::io::Result<()> {
        self.values
            .insert(key.to_owned(), Value::String(value.to_owned()));
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
    fn keeps_a_string() {
        let dir = std::env::temp_dir().join(format!("tw-settings-string-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"selectedEngine": 3}"#).unwrap();

        let mut settings = Settings::load_from(path.clone());
        assert_eq!(settings.string(SELECTED_ENGINE), None);
        settings
            .set_string(SELECTED_ENGINE, "com.typewhisper.whisper-cpp")
            .unwrap();
        settings.set_string(SELECTED_MODEL, "base").unwrap();
        let again = Settings::load_from(path);
        assert_eq!(
            again.string(SELECTED_ENGINE).as_deref(),
            Some("com.typewhisper.whisper-cpp")
        );
        assert_eq!(again.string(SELECTED_MODEL).as_deref(), Some("base"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keeps_the_settings_of_each_plugin_apart() {
        let dir = std::env::temp_dir().join(format!("tw-settings-plugin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"plugin.com.typewhisper.groq.beta.selectedModel": "x", "plugin.com.typewhisper.groqx.a": 1}"#,
        )
        .unwrap();

        let mut settings = Settings::load_from(path.clone());
        assert!(settings.plugin_values("com.typewhisper.groq").is_empty());
        settings
            .set_plugin_value(
                "com.typewhisper.groq",
                "selectedModel",
                "whisper-large-v3".into(),
            )
            .unwrap();
        let again = Settings::load_from(path.clone());
        let values = again.plugin_values("com.typewhisper.groq");
        assert_eq!(values.len(), 1);
        assert_eq!(values["selectedModel"], "whisper-large-v3");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"plugin.com.typewhisper.groq.selectedModel\""));

        let mut again = again;
        again
            .set_plugin_value("com.typewhisper.groq", "selectedModel", Value::Null)
            .unwrap();
        assert!(
            Settings::load_from(path)
                .plugin_values("com.typewhisper.groq")
                .is_empty()
        );
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
