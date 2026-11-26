//! Cloud engines such as Groq: the API key in the desktop's keyring, the
//! settings the plugin gets, and the app's own words for what went wrong at the
//! service, as the Windows app explains it.

use adw::prelude::*;
use tw_platform::i18n::{n_, tr, tr_with};
use tw_plugin_host::PluginError;
use tw_plugin_protocol::error_kind;

/// The keyring entry of a service's API key, the macOS plugins' secret name.
pub const API_KEY: &str = "api-key";

/// Where the group explains what leaves the computer.
const PRIVACY: &str = n_(
    "When {engine} is the selected engine, recorded audio is sent to {engine} for \
     transcription. The API key is stored in the desktop's keyring.",
);

/// The text the window and the indicator show for a plugin error. Errors of
/// cloud services come with a kind and get the Windows app's texts; any other
/// error keeps the plugin's own message.
pub fn explain(error: &PluginError, engine: &str) -> String {
    let PluginError::Plugin {
        kind: Some(kind),
        message,
        ..
    } = error
    else {
        return error.to_string();
    };
    let text = match kind.as_str() {
        error_kind::NOT_CONFIGURED => return tr("An API key is required for transcription."),
        error_kind::AUTHENTICATION => {
            n_("{engine} rejected the API key. Replace it in the TypeWhisper window.")
        }
        error_kind::PERMISSION => n_("This {engine} key does not have permission for the request."),
        error_kind::RATE_LIMIT => n_("{engine} rate limit reached. Wait and try again."),
        error_kind::NETWORK => n_("Could not reach {engine}. Check your internet connection."),
        error_kind::TIMEOUT => n_("{engine} timed out. Try again."),
        error_kind::REQUEST_TOO_LARGE => {
            n_("Recording exceeds {engine}'s upload limit. Use a shorter recording.")
        }
        error_kind::INVALID_REQUEST | error_kind::SERVER => {
            n_("{engine} could not complete the request. Check the selected model and try again.")
        }
        _ => return message.clone(),
    };
    tr_with(text, &[("engine", engine)])
}

/// The API key of the engine in use, below the engine picker. It shows only
/// for an engine that needs one.
pub struct ApiKeyGroup {
    pub group: adw::PreferencesGroup,
    entry: adw::PasswordEntryRow,
    remove: gtk::Button,
}

impl ApiKeyGroup {
    pub fn new() -> Self {
        let entry = adw::PasswordEntryRow::builder()
            .title(tr("API Key"))
            .show_apply_button(true)
            .build();
        entry.add_prefix(&gtk::Image::from_icon_name("dialog-password-symbolic"));
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text(tr("Remove API key"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .visible(false)
            .build();
        entry.add_suffix(&remove);
        let group = adw::PreferencesGroup::builder().visible(false).build();
        group.add(&entry);
        Self {
            group,
            entry,
            remove,
        }
    }

    /// Shows the group for `engine` with its saved key, or hides it when the
    /// engine needs no key.
    pub fn show(&self, engine: Option<&str>, key: Option<&str>) {
        let Some(engine) = engine else {
            self.group.set_visible(false);
            self.show_key(None);
            return;
        };
        self.group.set_title(engine);
        self.group
            .set_description(Some(&tr_with(PRIVACY, &[("engine", engine)])));
        self.show_key(key);
        self.group.set_visible(true);
    }

    /// Puts the saved key in the field, hidden behind dots as on macOS.
    pub fn show_key(&self, key: Option<&str>) {
        self.entry.set_text(key.unwrap_or_default());
        self.remove.set_visible(key.is_some());
    }

    /// The key can be changed only while nothing else runs.
    pub fn set_sensitive(&self, sensitive: bool) {
        self.entry.set_sensitive(sensitive);
    }

    /// `save` gets the entered key when the user applies it.
    pub fn connect_save(&self, save: impl Fn(String) + 'static) {
        self.entry
            .connect_apply(move |entry| save(entry.text().trim().to_owned()));
    }

    pub fn connect_remove(&self, remove: impl Fn() + 'static) {
        self.remove.connect_clicked(move |_| remove());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(kind: Option<&str>, message: &str) -> PluginError {
        PluginError::Plugin {
            code: -32020,
            message: message.into(),
            kind: kind.map(str::to_owned),
        }
    }

    #[test]
    fn explains_service_errors_in_the_apps_words() {
        let explained = |kind| explain(&failed(Some(kind), "Invalid API key"), "Groq");
        assert_eq!(
            explained(error_kind::AUTHENTICATION),
            "Groq rejected the API key. Replace it in the TypeWhisper window."
        );
        assert_eq!(
            explained(error_kind::REQUEST_TOO_LARGE),
            "Recording exceeds Groq's upload limit. Use a shorter recording."
        );
        assert_eq!(
            explained(error_kind::NOT_CONFIGURED),
            "An API key is required for transcription."
        );
        for kind in [error_kind::INVALID_REQUEST, error_kind::SERVER] {
            assert_eq!(
                explained(kind),
                "Groq could not complete the request. Check the selected model and try again."
            );
        }
    }

    #[test]
    fn keeps_the_plugins_message_otherwise() {
        assert_eq!(
            explain(&failed(Some("somethingNew"), "Odd failure"), "Groq"),
            "Odd failure"
        );
        assert_eq!(
            explain(&failed(None, "no model loaded"), "Groq"),
            "no model loaded"
        );
    }
}
