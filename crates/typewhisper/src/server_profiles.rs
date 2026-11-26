//! Server profiles of the OpenAI-compatible engine: the list in the main
//! window and the editor, as in the Windows app. Each profile has its own URL,
//! optional API key, models and options. The profiles go into the settings under
//! the plugin's keys, the keys into the desktop's keyring.
//!
//! The editor's actions (the connection test, model discovery and a test
//! prompt) use the values in the editor without saving them, through a plugin
//! process of their own.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use serde_json::{Value, json};
use tw_openai_compatible::profile::{
    DEFAULT_TEMPERATURE, MAX_TEMPERATURE, MAX_TEXT_TIMEOUT_SECONDS, MIN_TEXT_TIMEOUT_SECONDS,
};
use tw_openai_compatible::server_url::{self, UrlError};
use tw_openai_compatible::{
    BatchEndpoint, FetchedModel, PLUGIN_ID, Profile, ProfileError, Profiles, RealtimeProtocol,
    ReasoningEffort, TemperatureMode, TextApi, Transport, secret_name,
};
use tw_platform::i18n::{n_, tr, tr_with};
use tw_platform::secrets::SecretStore;
use tw_plugin_host::{InstalledPlugin, PluginError, PluginProcess};
use tw_plugin_protocol::{PluginSettings, error_kind};

/// The plugin's setting with the profiles, under the macOS name.
pub const PROFILES: &str = "profiles";

/// The name of a new profile, then "Custom Server 2" and so on, as on Windows.
const NEW_PROFILE_NAME: &str = n_("Custom Server");

/// What the app does with the profiles: save them, and pass them on to a
/// running plugin.
pub trait Host {
    fn save(&self, profiles: &Value) -> std::io::Result<()>;
    /// The profiles or a key changed.
    fn changed(&self, settings: &PluginSettings);
}

/// The "OpenAI Compatible" group of the main window.
pub struct ServerProfiles {
    group: adw::PreferencesGroup,
    window: adw::ApplicationWindow,
    plugin: InstalledPlugin,
    profiles: RefCell<Profiles>,
    /// Keys by secret name, as the keyring had them and as saved since.
    keys: RefCell<BTreeMap<String, String>>,
    keys_loaded: Cell<bool>,
    secrets: RefCell<Option<Rc<SecretStore>>>,
    /// Why the keyring cannot be used, if it cannot.
    keyring_error: RefCell<Option<String>>,
    rows: RefCell<Vec<adw::ActionRow>>,
    /// The open editor; the dialog's handlers only hold it weakly.
    editor: RefCell<Option<Rc<Editor>>>,
    host: Box<dyn Host>,
}

impl ServerProfiles {
    /// Fills `group`. `values` are the plugin's saved settings.
    pub fn new(
        group: &adw::PreferencesGroup,
        window: &adw::ApplicationWindow,
        plugin: InstalledPlugin,
        values: &serde_json::Map<String, Value>,
        host: impl Host + 'static,
    ) -> Rc<Self> {
        let add = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text(tr("Add Profile"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        group.set_header_suffix(Some(&add));
        let this = Rc::new(Self {
            group: group.clone(),
            window: window.clone(),
            plugin,
            profiles: RefCell::new(Profiles::from_value(values.get(PROFILES))),
            keys: RefCell::default(),
            keys_loaded: Cell::new(false),
            secrets: RefCell::default(),
            keyring_error: RefCell::default(),
            rows: RefCell::default(),
            editor: RefCell::default(),
            host: Box::new(host),
        });
        this.show_rows();
        add.connect_clicked(glib::clone!(
            #[weak]
            this,
            move |_| {
                let profile = this.profiles.borrow().clone().add(&tr(NEW_PROFILE_NAME));
                glib::spawn_future_local(Rc::clone(&this).edit(profile, true));
            }
        ));
        this
    }

    async fn edit(self: Rc<Self>, profile: Profile, is_new: bool) {
        if self.editor.borrow().is_some() {
            return;
        }
        self.load_keys().await;
        Editor::open(&self, profile, is_new);
    }

    /// Reads the keys from the keyring, once it succeeds. Only when they are
    /// needed, so a locked keyring asks for its password only then.
    pub async fn load_keys(&self) {
        if self.keys_loaded.replace(true) {
            return;
        }
        // After a failure, for example a cancelled unlock prompt, the next
        // action that needs the keys tries again.
        let opened = match SecretStore::open().await {
            Ok(store) => store,
            Err(error) => {
                eprintln!("typewhisper: no keyring for the server keys: {error}");
                *self.keyring_error.borrow_mut() = Some(error.to_string());
                self.keys_loaded.set(false);
                return;
            }
        };
        match opened.load_all(PLUGIN_ID).await {
            Ok(keys) => {
                *self.keys.borrow_mut() = keys;
                *self.keyring_error.borrow_mut() = None;
            }
            Err(error) => {
                eprintln!("typewhisper: the server keys could not be read: {error}");
                *self.keyring_error.borrow_mut() = Some(error.to_string());
                self.keys_loaded.set(false);
            }
        }
        *self.secrets.borrow_mut() = Some(Rc::new(opened));
    }

    /// What the plugin gets with `settings.update`.
    pub fn plugin_settings(&self) -> PluginSettings {
        let mut values = serde_json::Map::new();
        values.insert(PROFILES.into(), self.profiles.borrow().to_value());
        PluginSettings {
            values,
            secrets: self.keys.borrow().clone(),
        }
    }

    fn show_rows(self: &Rc<Self>) {
        for row in self.rows.take() {
            self.group.remove(&row);
        }
        let mut rows = Vec::new();
        for profile in self.profiles.borrow().iter() {
            let subtitle = match (
                profile.base_url.as_str(),
                profile.selected_model_id.as_str(),
            ) {
                ("", _) => tr("Not set up"),
                (url, "") => url.to_owned(),
                (url, model) => format!("{url} · {model}"),
            };
            let row = adw::ActionRow::builder()
                .title(profile.display_name())
                .subtitle(subtitle)
                .use_markup(false)
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("network-server-symbolic"));
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let id = profile.id.clone();
            row.connect_activated(glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |_| {
                    let profile = this.profiles.borrow().get(&id).cloned();
                    if let Some(profile) = profile {
                        glib::spawn_future_local(Rc::clone(&this).edit(profile, false));
                    }
                }
            ));
            self.group.add(&row);
            rows.push(row);
        }
        *self.rows.borrow_mut() = rows;
    }

    /// The server URL of a profile.
    pub fn base_url(&self, profile_id: &str) -> Option<String> {
        self.profiles
            .borrow()
            .get(profile_id)
            .map(|p| p.base_url.clone())
    }

    fn saved_key(&self, profile_id: &str) -> Option<String> {
        self.keys.borrow().get(&secret_name(profile_id)).cloned()
    }

    /// Saves the key change first, so that a keyring failure changes nothing.
    async fn save(self: &Rc<Self>, profile: Profile, key: KeyChange) -> Result<(), String> {
        let entry = secret_name(&profile.id);
        if !matches!(key, KeyChange::Keep) {
            // The keyring may have failed when the editor opened; try it again.
            let missing = self.secrets.borrow().is_none();
            if missing {
                self.load_keys().await;
            }
            let store = self.secrets.borrow().clone().ok_or_else(|| {
                let reason = self.keyring_error.borrow().clone().unwrap_or_default();
                tr_with(
                    "The API key could not be saved: {error}",
                    &[("error", &reason)],
                )
            })?;
            let result = match &key {
                KeyChange::Set(secret) => store.store(PLUGIN_ID, &entry, secret).await,
                KeyChange::Remove => store.delete(PLUGIN_ID, &entry).await,
                KeyChange::Keep => Ok(()),
            };
            result.map_err(|error| {
                tr_with(
                    "The API key could not be saved: {error}",
                    &[("error", &error.to_string())],
                )
            })?;
            match key {
                KeyChange::Set(secret) => {
                    self.keys
                        .borrow_mut()
                        .insert(entry, secret.trim().to_owned());
                }
                KeyChange::Remove => {
                    self.keys.borrow_mut().remove(&entry);
                }
                KeyChange::Keep => {}
            }
        }
        let mut profiles = self.profiles.borrow().clone();
        profiles.insert(profile);
        self.commit(profiles)
    }

    fn commit(self: &Rc<Self>, profiles: Profiles) -> Result<(), String> {
        self.host.save(&profiles.to_value()).map_err(|error| {
            tr_with(
                "The setting could not be saved: {error}",
                &[("error", &error.to_string())],
            )
        })?;
        *self.profiles.borrow_mut() = profiles;
        self.show_rows();
        self.host.changed(&self.plugin_settings());
        Ok(())
    }

    /// Removes the profile, then its key. A key that cannot be removed stays in
    /// the keyring, unused.
    async fn delete(self: &Rc<Self>, id: &str) -> Result<(), String> {
        let mut profiles = self.profiles.borrow().clone();
        if !profiles.remove(id) {
            return Ok(());
        }
        self.commit(profiles)?;
        let entry = secret_name(id);
        let store = self.secrets.borrow().clone();
        if let Some(store) = store
            && let Err(error) = store.delete(PLUGIN_ID, &entry).await
        {
            eprintln!("typewhisper: the removed profile's key stays in the keyring: {error}");
        }
        self.keys.borrow_mut().remove(&entry);
        self.host.changed(&self.plugin_settings());
        Ok(())
    }
}

enum KeyChange {
    Keep,
    Set(String),
    Remove,
}

/// A combo row whose items stand for raw values.
struct Choice {
    row: adw::ComboRow,
    values: Vec<&'static str>,
}

impl Choice {
    fn new(title: &str, subtitle: &str, items: &[(&'static str, String)], current: &str) -> Self {
        let labels: Vec<&str> = items.iter().map(|(_, label)| label.as_str()).collect();
        let row = adw::ComboRow::builder()
            .title(title)
            .model(&gtk::StringList::new(&labels))
            .build();
        if !subtitle.is_empty() {
            row.set_subtitle(subtitle);
        }
        let values: Vec<&'static str> = items.iter().map(|(value, _)| *value).collect();
        let index = values.iter().position(|v| *v == current).unwrap_or(0);
        row.set_selected(index as u32);
        Self { row, values }
    }

    fn value(&self) -> &'static str {
        self.values
            .get(self.row.selected() as usize)
            .copied()
            .unwrap_or(self.values[0])
    }
}

/// The outcome of an editor action, in a row below its button, so that long
/// texts such as a server's error wrap instead of being cut off.
struct Status {
    row: adw::ActionRow,
    icon: gtk::Image,
}

enum Outcome {
    Success,
    Warning,
    Failure,
}

impl Status {
    /// `selectable` for a text worth copying, such as a model's answer.
    fn new(selectable: bool) -> Self {
        let icon = gtk::Image::new();
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .title_lines(0)
            .title_selectable(selectable)
            .visible(false)
            .build();
        row.add_prefix(&icon);
        Self { row, icon }
    }

    fn show(&self, outcome: Outcome, text: &str, subtitle: &str) {
        let (icon, tone) = match outcome {
            Outcome::Success => ("object-select-symbolic", "success"),
            Outcome::Warning => ("dialog-warning-symbolic", "warning"),
            Outcome::Failure => ("dialog-error-symbolic", "error"),
        };
        self.icon.set_icon_name(Some(icon));
        self.icon.set_css_classes(&[tone]);
        self.row.set_title(text);
        self.row.set_subtitle(subtitle);
        self.row.set_visible(true);
    }
}

/// An entry for a model ID with a menu of the server's models.
struct ModelField {
    row: adw::EntryRow,
    list: gtk::ListBox,
    button: gtk::MenuButton,
}

impl ModelField {
    fn new(title: &str, current: &str) -> Self {
        let row = adw::EntryRow::builder().title(title).text(current).build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["navigation-sidebar"])
            .build();
        let scrolled = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(320)
            .build();
        let popover = gtk::Popover::builder().child(&scrolled).build();
        let button = gtk::MenuButton::builder()
            .icon_name("view-list-symbolic")
            .tooltip_text(tr("Models"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .popover(&popover)
            .build();
        row.add_suffix(&button);
        list.connect_row_activated(glib::clone!(
            #[weak]
            row,
            #[weak]
            popover,
            move |_, item| {
                if let Some(label) = item.child().and_downcast::<gtk::Label>() {
                    row.set_text(&label.label());
                }
                popover.popdown();
            }
        ));
        Self { row, list, button }
    }

    fn show_models(&self, models: &[String]) {
        self.list.remove_all();
        for model in models {
            let label = gtk::Label::builder()
                .label(model)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .build();
            self.list.append(&label);
        }
        self.button.set_sensitive(!models.is_empty());
    }

    fn text(&self) -> String {
        self.row.text().trim().to_owned()
    }
}

/// The plugin process of an open editor, started on its first action.
type EditorPlugin = Arc<Mutex<Option<PluginProcess>>>;

struct Editor {
    profiles: Rc<ServerProfiles>,
    dialog: adw::Dialog,
    toasts: adw::ToastOverlay,
    original: Profile,
    is_new: bool,
    saved_key: Option<String>,
    remove_key: Cell<bool>,
    /// Models the server listed while the editor is open, for the URL and API
    /// version they came from.
    catalog: RefCell<Option<(String, String, Vec<FetchedModel>)>>,
    plugin: EditorPlugin,
    name: adw::EntryRow,
    url: adw::EntryRow,
    key: adw::PasswordEntryRow,
    key_saved: gtk::Box,
    key_hint: adw::ActionRow,
    api_version: adw::EntryRow,
    transcription_model: ModelField,
    transport: Choice,
    realtime_protocol: Choice,
    batch_endpoint: Choice,
    text_model: ModelField,
    text_api: Choice,
    reasoning: Choice,
    temperature_mode: Choice,
    temperature: adw::SpinRow,
    thinking: adw::SwitchRow,
    timeout: adw::SpinRow,
    connection_status: Status,
    models_status: Status,
    answer: Status,
    busy: Cell<bool>,
    actions: Vec<adw::ButtonRow>,
}

impl Editor {
    fn open(profiles: &Rc<ServerProfiles>, profile: Profile, is_new: bool) {
        let saved_key = profiles.saved_key(&profile.id);
        let p = &profile;

        let name = adw::EntryRow::builder()
            .title(tr("Profile Name"))
            .text(&p.name)
            .build();
        let url = adw::EntryRow::builder()
            .title(tr("Server URL"))
            .text(&p.base_url)
            .input_purpose(gtk::InputPurpose::Url)
            .build();
        let key = adw::PasswordEntryRow::builder()
            .title(tr("API Key"))
            .build();
        let key_saved = gtk::Box::builder()
            .spacing(6)
            .valign(gtk::Align::Center)
            .build();
        key_saved.append(
            &gtk::Label::builder()
                .label(tr("Saved"))
                .css_classes(["dim-label"])
                .build(),
        );
        let remove_key = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text(tr("Remove API key"))
            .css_classes(["flat"])
            .build();
        key_saved.append(&remove_key);
        key_saved.set_visible(saved_key.is_some());
        key.add_suffix(&key_saved);
        let key_hint = adw::ActionRow::builder()
            .title(tr(
                "This server gets no API key: keys are sent only over HTTPS or to this computer.",
            ))
            .title_lines(0)
            .css_classes(["warning"])
            .build();
        key_hint.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        let api_version = adw::EntryRow::builder()
            .title(tr("API Version (optional)"))
            .text(&p.api_version)
            .build();
        let test = adw::ButtonRow::builder()
            .title(tr("Test Connection"))
            .start_icon_name("network-transmit-receive-symbolic")
            .build();
        let connection_status = Status::new(false);
        let connection = adw::PreferencesGroup::builder()
            .title(tr("Connection"))
            .description(tr(
                "For example http://localhost:1234 for LM Studio or http://localhost:11434 for \
                 Ollama; a trailing /v1 is optional. The API key is optional for local servers. \
                 It is sent only over HTTPS or to this computer.",
            ))
            .build();
        for row in [
            name.upcast_ref::<gtk::Widget>(),
            url.upcast_ref(),
            key.upcast_ref(),
            key_hint.upcast_ref(),
            api_version.upcast_ref(),
            test.upcast_ref(),
            connection_status.row.upcast_ref(),
        ] {
            connection.add(row);
        }

        let auto = || tr("Auto");
        let transcription_model = ModelField::new(&tr("Transcription Model"), &p.selected_model_id);
        let transport = Choice::new(
            &tr("Transcription Transport"),
            &tr(
                "Auto uses Realtime for gpt-live-transcribe and gpt-realtime-whisper, Batch for other models.",
            ),
            &[
                (Transport::Auto.as_str(), auto()),
                (Transport::Batch.as_str(), tr("Batch")),
                (Transport::Realtime.as_str(), tr("Realtime")),
            ],
            &p.transcription_transport_raw,
        );
        let realtime_protocol = Choice::new(
            &tr("Realtime Protocol"),
            &tr("For a deployment alias, the model family behind it."),
            &[
                (RealtimeProtocol::Auto.as_str(), auto()),
                (
                    RealtimeProtocol::Live.as_str(),
                    "Live Transcribe".to_owned(),
                ),
                (
                    RealtimeProtocol::Whisper.as_str(),
                    "Realtime Whisper".to_owned(),
                ),
            ],
            &p.realtime_protocol_raw,
        );
        let batch_endpoint = Choice::new(
            &tr("Batch Transcription Endpoint"),
            &tr("Deployment-scoped needs a dated API version."),
            &[
                (BatchEndpoint::Standard.as_str(), tr("Standard v1")),
                (
                    BatchEndpoint::DeploymentScoped.as_str(),
                    tr("Deployment-scoped"),
                ),
            ],
            &p.batch_endpoint_raw,
        );
        let refresh_models = adw::ButtonRow::builder()
            .title(tr("Refresh Models"))
            .start_icon_name("view-refresh-symbolic")
            .build();
        let models_status = Status::new(false);
        let transcription = adw::PreferencesGroup::builder()
            .title(tr("Transcription"))
            .description(tr(
                "When this profile is the engine, recorded audio is sent to this server for \
                 transcription.",
            ))
            .build();
        for row in [
            transcription_model.row.upcast_ref::<gtk::Widget>(),
            transport.row.upcast_ref(),
            realtime_protocol.row.upcast_ref(),
            batch_endpoint.row.upcast_ref(),
            refresh_models.upcast_ref(),
            models_status.row.upcast_ref(),
        ] {
            transcription.add(row);
        }

        let provider_default = || tr("Provider Default");
        let text_model = ModelField::new(&tr("LLM Model"), &p.selected_llm_model_id);
        let text_api = Choice::new(
            &tr("LLM API"),
            &tr("Choose the API supported by your server."),
            &[
                (
                    TextApi::ChatCompletions.as_str(),
                    "Chat Completions".to_owned(),
                ),
                (TextApi::Responses.as_str(), "Responses".to_owned()),
            ],
            &p.llm_api_mode_raw,
        );
        let reasoning = Choice::new(
            &tr("Reasoning Effort"),
            &tr(
                "Provider Default omits reasoning. Other values send reasoning: { effort: ... } \
                 to /v1/responses.",
            ),
            &[
                (
                    ReasoningEffort::ProviderDefault.as_str(),
                    provider_default(),
                ),
                (ReasoningEffort::Low.as_str(), tr("Low")),
                (ReasoningEffort::Medium.as_str(), tr("Medium")),
                (ReasoningEffort::High.as_str(), tr("High")),
                (ReasoningEffort::XHigh.as_str(), tr("X High")),
                (ReasoningEffort::Max.as_str(), tr("Max")),
            ],
            &p.reasoning_effort_raw,
        );
        let temperature_mode = Choice::new(
            &tr("Temperature"),
            &tr("Provider Default sends no temperature override."),
            &[
                (
                    TemperatureMode::ProviderDefault.as_str(),
                    provider_default(),
                ),
                (TemperatureMode::Custom.as_str(), tr("Custom")),
            ],
            &p.llm_temperature_mode_raw,
        );
        let temperature = adw::SpinRow::with_range(0.0, MAX_TEMPERATURE, 0.1);
        temperature.set_title(&tr("Temperature Value"));
        temperature.set_subtitle(&tr(
            "From 0 to 2. Lower values give more consistent results.",
        ));
        temperature.set_digits(1);
        temperature.set_value(if p.llm_temperature_value.is_finite() {
            p.llm_temperature_value
        } else {
            DEFAULT_TEMPERATURE
        });
        let thinking = adw::SwitchRow::builder()
            .title(tr("Provider Thinking Extension"))
            .subtitle(tr(
                "Uses reasoning_effort for DeepInfra and thinking.type for other compatible servers. \
                 Leave off unless your provider documents support.",
            ))
            .active(p.thinking_enabled)
            .build();
        let timeout = adw::SpinRow::with_range(
            f64::from(MIN_TEXT_TIMEOUT_SECONDS),
            f64::from(MAX_TEXT_TIMEOUT_SECONDS),
            5.0,
        );
        timeout.set_title(&tr("LLM Request Timeout"));
        timeout.set_subtitle(&tr(
            "Seconds, from 5 to 3600. Local models may need more time.",
        ));
        timeout.set_value(f64::from(p.text_timeout_seconds()));
        let test_prompt = adw::ButtonRow::builder()
            .title(tr("Send Test Prompt"))
            .start_icon_name("mail-send-symbolic")
            .build();
        let answer = Status::new(true);
        let text = adw::PreferencesGroup::builder()
            .title(tr("Language Model"))
            .description(tr(
                "For text processing. The test prompt sends a short question to the LLM model \
                 above.",
            ))
            .build();
        for row in [
            text_model.row.upcast_ref::<gtk::Widget>(),
            text_api.row.upcast_ref(),
            reasoning.row.upcast_ref(),
            temperature_mode.row.upcast_ref(),
            temperature.upcast_ref(),
            thinking.upcast_ref(),
            timeout.upcast_ref(),
            test_prompt.upcast_ref(),
            answer.row.upcast_ref(),
        ] {
            text.add(row);
        }

        let page = adw::PreferencesPage::new();
        page.add(&connection);
        page.add(&transcription);
        page.add(&text);
        let delete = adw::ButtonRow::builder()
            .title(tr("Delete Profile"))
            .css_classes(["destructive-action"])
            .build();
        if !is_new && !p.is_default() {
            let danger = adw::PreferencesGroup::new();
            danger.add(&delete);
            page.add(&danger);
        }

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&page));
        let cancel = gtk::Button::with_label(&tr("Cancel"));
        let save = gtk::Button::builder()
            .label(tr("Save"))
            .css_classes(["suggested-action"])
            .build();
        let header = adw::HeaderBar::builder()
            .show_start_title_buttons(false)
            .show_end_title_buttons(false)
            .build();
        header.pack_start(&cancel);
        header.pack_end(&save);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&toasts));
        let dialog = adw::Dialog::builder()
            .title(if is_new {
                tr("New Profile")
            } else {
                p.display_name().to_owned()
            })
            .content_width(560)
            .content_height(720)
            .can_close(false)
            .child(&view)
            .build();

        let editor = Rc::new(Self {
            profiles: Rc::clone(profiles),
            dialog: dialog.clone(),
            toasts,
            original: profile.clone(),
            is_new,
            saved_key,
            remove_key: Cell::new(false),
            catalog: RefCell::new(None),
            plugin: EditorPlugin::default(),
            name,
            url,
            key,
            key_saved,
            key_hint,
            api_version,
            transcription_model,
            transport,
            realtime_protocol,
            batch_endpoint,
            text_model,
            text_api,
            reasoning,
            temperature_mode,
            temperature,
            thinking,
            timeout,
            connection_status,
            models_status,
            answer,
            busy: Cell::new(false),
            actions: vec![test.clone(), refresh_models.clone(), test_prompt.clone()],
        });
        editor.show_models(&profile.model_suggestions());
        editor.refresh();

        let refresh_on = |editor: &Rc<Self>| {
            let weak = Rc::downgrade(editor);
            move || {
                if let Some(editor) = weak.upgrade() {
                    editor.refresh();
                }
            }
        };
        editor.name.connect_changed({
            let refresh = refresh_on(&editor);
            move |_| refresh()
        });
        // Results for another server or key no longer apply.
        let stale = |editor: &Rc<Self>| {
            let weak = Rc::downgrade(editor);
            move || {
                if let Some(editor) = weak.upgrade() {
                    editor.connection_status.row.set_visible(false);
                    editor.models_status.row.set_visible(false);
                    editor.refresh();
                }
            }
        };
        editor.url.connect_changed({
            let stale = stale(&editor);
            move |_| stale()
        });
        editor.key.connect_changed({
            let stale = stale(&editor);
            move |_| stale()
        });
        for choice in [
            &editor.transport,
            &editor.text_api,
            &editor.temperature_mode,
        ] {
            let refresh = refresh_on(&editor);
            choice.row.connect_selected_notify(move |_| refresh());
        }
        remove_key.connect_clicked(glib::clone!(
            #[weak]
            editor,
            move |_| {
                editor.remove_key.set(true);
                editor.key.set_text("");
                editor.refresh();
            }
        ));
        test.connect_activated(glib::clone!(
            #[weak]
            editor,
            move |_| {
                glib::spawn_future_local(Rc::clone(&editor).test_connection());
            }
        ));
        refresh_models.connect_activated(glib::clone!(
            #[weak]
            editor,
            move |_| {
                glib::spawn_future_local(Rc::clone(&editor).refresh_models());
            }
        ));
        test_prompt.connect_activated(glib::clone!(
            #[weak]
            editor,
            move |_| {
                glib::spawn_future_local(Rc::clone(&editor).test_prompt());
            }
        ));
        delete.connect_activated(glib::clone!(
            #[weak]
            editor,
            move |_| {
                glib::spawn_future_local(Rc::clone(&editor).confirm_delete());
            }
        ));
        cancel.connect_clicked(glib::clone!(
            #[weak]
            editor,
            move |_| {
                glib::spawn_future_local(Rc::clone(&editor).close());
            }
        ));
        save.connect_clicked(glib::clone!(
            #[weak]
            editor,
            move |_| {
                glib::spawn_future_local(Rc::clone(&editor).save());
            }
        ));
        dialog.connect_close_attempt(glib::clone!(
            #[weak]
            editor,
            move |_| {
                glib::spawn_future_local(Rc::clone(&editor).close());
            }
        ));
        dialog.connect_closed(glib::clone!(
            #[weak]
            profiles,
            move |_| {
                if let Some(editor) = profiles.editor.take() {
                    // Ends the editor's plugin process off the main thread.
                    let plugin = Arc::clone(&editor.plugin);
                    std::thread::spawn(move || drop(plugin.lock().map(|mut p| p.take())));
                }
            }
        ));
        *profiles.editor.borrow_mut() = Some(Rc::clone(&editor));
        dialog.present(Some(&profiles.window));
    }

    /// Shows the rows that apply to the current choices.
    fn refresh(&self) {
        let transport = Transport::parse(self.transport.value());
        self.realtime_protocol
            .row
            .set_visible(transport != Transport::Batch);
        self.batch_endpoint
            .row
            .set_visible(transport != Transport::Realtime);
        let responses = TextApi::parse(self.text_api.value()) == TextApi::Responses;
        self.reasoning.row.set_visible(responses);
        self.thinking.set_visible(!responses);
        self.temperature.set_visible(
            TemperatureMode::parse(self.temperature_mode.value()) == TemperatureMode::Custom,
        );
        let has_key = !self.key.text().trim().is_empty()
            || (self.saved_key.is_some() && !self.remove_key.get());
        self.key_saved
            .set_visible(self.saved_key.is_some() && !self.remove_key.get());
        let url = server_url::normalize(&self.url.text());
        self.key_hint
            .set_visible(has_key && !url.is_empty() && !server_url::sends_api_key_to(&url));
        for action in &self.actions {
            action.set_sensitive(!self.busy.get());
        }
    }

    fn show_models(&self, models: &[String]) {
        self.transcription_model.show_models(models);
        self.text_model.show_models(models);
    }

    /// The profile as the editor shows it.
    fn draft(&self) -> Profile {
        let mut profile = self.original.clone();
        profile.name = self.name.text().trim().to_owned();
        profile.base_url = self.url.text().trim().to_owned();
        profile.api_version = self.api_version.text().trim().to_owned();
        profile.selected_model_id = self.transcription_model.text();
        profile.selected_llm_model_id = self.text_model.text();
        profile.transcription_transport_raw = self.transport.value().to_owned();
        profile.realtime_protocol_raw = self.realtime_protocol.value().to_owned();
        profile.batch_endpoint_raw = self.batch_endpoint.value().to_owned();
        profile.llm_api_mode_raw = self.text_api.value().to_owned();
        profile.reasoning_effort_raw = self.reasoning.value().to_owned();
        profile.llm_temperature_mode_raw = self.temperature_mode.value().to_owned();
        profile.llm_temperature_value = (self.temperature.value() * 10.0).round() / 10.0;
        profile.thinking_enabled = self.thinking.is_active();
        let seconds = self.timeout.value().round();
        profile.chat_request_timeout_seconds = (seconds
            != f64::from(self.original.text_timeout_seconds())
            || self.original.chat_request_timeout_seconds.is_some())
        .then_some(seconds);
        let url = server_url::normalize(&profile.base_url);
        let catalog = self.catalog.borrow();
        if let Some((from_url, from_version, models)) = catalog.as_ref()
            && *from_url == url
            && *from_version == profile.api_version
        {
            profile.fetched_models = models.clone();
        } else if url != server_url::normalize(&self.original.base_url)
            || profile.api_version != self.original.api_version
        {
            // The old server's models do not belong to the new one.
            profile.fetched_models.clear();
        }
        profile.normalize();
        profile
    }

    /// The key a request uses: the one typed in, else the saved one.
    fn draft_key(&self) -> Option<String> {
        let typed = self.key.text().trim().to_owned();
        if !typed.is_empty() {
            return Some(typed);
        }
        self.saved_key.clone().filter(|_| !self.remove_key.get())
    }

    fn key_change(&self) -> KeyChange {
        let typed = self.key.text().trim().to_owned();
        if !typed.is_empty() {
            KeyChange::Set(typed)
        } else if self.remove_key.get() {
            KeyChange::Remove
        } else {
            KeyChange::Keep
        }
    }

    fn is_dirty(&self) -> bool {
        let mut original = self.original.clone();
        original.normalize();
        self.is_new || self.draft() != original || !matches!(self.key_change(), KeyChange::Keep)
    }

    fn toast(&self, text: &str) {
        self.toasts.add_toast(
            adw::Toast::builder()
                .title(text)
                .use_markup(false)
                .timeout(5)
                .build(),
        );
    }

    /// Checks the URL before a request, so the user sees what is wrong.
    fn checked_draft(&self) -> Option<Profile> {
        let mut profile = self.draft();
        match server_url::validate(&profile.base_url) {
            Ok(url) if url.is_empty() => {
                self.toast(&tr("Enter the server URL first."));
                None
            }
            Ok(url) => {
                profile.base_url = url;
                Some(profile)
            }
            Err(error) => {
                self.toast(&url_error_text(error));
                None
            }
        }
    }

    /// Runs one plugin request with the editor's values off the main thread.
    async fn request(
        &self,
        method: &'static str,
        profile: &Profile,
        extra: Value,
        limit: Duration,
    ) -> Result<Value, PluginError> {
        let mut params = json!({"profile": profile, "apiKey": self.draft_key()});
        if let (Value::Object(params), Value::Object(extra)) = (&mut params, extra) {
            params.extend(extra);
        }
        let plugin = Arc::clone(&self.plugin);
        let installed = self.profiles.plugin.clone();
        self.busy.set(true);
        self.refresh();
        let result = gtk::gio::spawn_blocking(move || {
            let mut slot = plugin
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.is_none() {
                let data = tw_platform::data_dir()?.join("plugin-data").join(PLUGIN_ID);
                let (process, _) =
                    PluginProcess::start(&installed, &data, env!("CARGO_PKG_VERSION"))?;
                *slot = Some(process);
            }
            let process = slot.as_mut().expect("started above");
            let result = process.call_with_limit(method, params, limit, &mut |_, _| {});
            if matches!(
                result,
                Err(PluginError::Io(_) | PluginError::Protocol(_) | PluginError::TimedOut(_))
            ) {
                // A new process for the next request.
                *slot = None;
            }
            result
        })
        .await
        .unwrap_or_else(|_| Err(PluginError::Protocol("the plugin host crashed".into())));
        self.busy.set(false);
        self.refresh();
        result
    }

    async fn test_connection(self: Rc<Self>) {
        let Some(profile) = self.checked_draft() else {
            return;
        };
        match self
            .request("connection.test", &profile, json!({}), EDITOR_REQUEST_TIME)
            .await
        {
            Ok(result) if result["keyWithheld"].as_bool() == Some(true) => {
                self.connection_status.show(
                    Outcome::Warning,
                    &tr(
                        "Connection verified without the API key: it is sent only over HTTPS or \
                         to this computer.",
                    ),
                    "",
                );
            }
            Ok(_) => {
                self.connection_status
                    .show(Outcome::Success, &tr("Connection verified."), "");
            }
            Err(error) => self.connection_status.show(
                Outcome::Failure,
                &explain(&error, profile.display_name()),
                "",
            ),
        }
    }

    async fn refresh_models(self: Rc<Self>) {
        let Some(profile) = self.checked_draft() else {
            return;
        };
        let result = self
            .request("models.fetch", &profile, json!({}), EDITOR_REQUEST_TIME)
            .await;
        let models: Vec<FetchedModel> = match result {
            Ok(result) => serde_json::from_value(result["models"].clone()).unwrap_or_default(),
            Err(error) => {
                self.models_status.show(
                    Outcome::Failure,
                    &explain(&error, profile.display_name()),
                    "",
                );
                return;
            }
        };
        if models.is_empty() {
            self.models_status.show(
                Outcome::Warning,
                &tr("No models returned. You can enter model IDs manually."),
                "",
            );
            return;
        }
        let count = models.len().to_string();
        *self.catalog.borrow_mut() = Some((
            profile.base_url.clone(),
            profile.api_version.clone(),
            models,
        ));
        self.show_models(&self.draft().model_suggestions());
        self.models_status.show(
            Outcome::Success,
            &tr_with(
                "{count} models found. Choose models and save the profile.",
                &[("count", &count)],
            ),
            "",
        );
    }

    async fn test_prompt(self: Rc<Self>) {
        let Some(profile) = self.checked_draft() else {
            return;
        };
        if profile.selected_llm_model_id.is_empty() {
            self.toast(&tr("Enter an LLM model first."));
            return;
        }
        let limit = Duration::from_secs(u64::from(profile.text_timeout_seconds())) + PLUGIN_MARGIN;
        let prompt = json!({
            "systemPrompt": "You are a helpful assistant. Answer in one short sentence.",
            "userText": "Say hello and name the model you are.",
        });
        match self.request("llm.process", &profile, prompt, limit).await {
            Ok(result) => self.answer.show(
                Outcome::Success,
                result["text"].as_str().unwrap_or_default(),
                &tr_with(
                    "Answer from {model}",
                    &[("model", &profile.selected_llm_model_id)],
                ),
            ),
            Err(error) => self.answer.show(
                Outcome::Failure,
                &explain(&error, profile.display_name()),
                "",
            ),
        }
    }

    async fn save(self: Rc<Self>) {
        let mut profile = self.draft();
        if let Err(error) = profile.validate() {
            self.toast(&profile_error_text(error));
            return;
        }
        match self.profiles.save(profile, self.key_change()).await {
            Ok(()) => {
                self.dialog.force_close();
            }
            Err(error) => self.toast(&error),
        }
    }

    /// Closes, after asking when there are unsaved changes.
    async fn close(self: Rc<Self>) {
        if !self.is_dirty() {
            self.dialog.force_close();
            return;
        }
        let alert = adw::AlertDialog::new(
            Some(&tr("Discard Changes?")),
            Some(&tr("The changes to this profile are not saved.")),
        );
        alert.add_responses(&[("keep", &tr("Keep Editing")), ("discard", &tr("Discard"))]);
        alert.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        alert.set_default_response(Some("keep"));
        alert.set_close_response("keep");
        if alert.choose_future(Some(&self.dialog)).await == "discard" {
            self.dialog.force_close();
        }
    }

    async fn confirm_delete(self: Rc<Self>) {
        let alert = adw::AlertDialog::new(
            Some(&tr_with(
                "Delete “{name}”?",
                &[("name", self.original.display_name())],
            )),
            Some(&tr("The profile and its saved API key are removed.")),
        );
        alert.add_responses(&[("cancel", &tr("Cancel")), ("delete", &tr("Delete"))]);
        alert.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        alert.set_default_response(Some("cancel"));
        alert.set_close_response("cancel");
        if alert.choose_future(Some(&self.dialog)).await != "delete" {
            return;
        }
        match self.profiles.delete(&self.original.id).await {
            Ok(()) => self.dialog.force_close(),
            Err(error) => self.toast(&error),
        }
    }
}

/// The editor's requests: the plugin's 15 seconds and time to start it.
const EDITOR_REQUEST_TIME: Duration = Duration::from_secs(25);
/// Time the plugin gets beyond its own timeout, so it reports a slow server.
const PLUGIN_MARGIN: Duration = Duration::from_secs(10);

fn url_error_text(error: UrlError) -> String {
    match error {
        UrlError::Scheme => tr("Enter a server URL that starts with http:// or https://."),
        UrlError::Credentials => {
            tr("Remove the user name and password from the server URL. Use the API key field.")
        }
        UrlError::QueryOrFragment => tr(
            "Remove the part after ? or # from the server URL. The API version has its own field.",
        ),
        UrlError::Host | UrlError::Port | UrlError::Characters => tr("The server URL is invalid."),
    }
}

fn profile_error_text(error: ProfileError) -> String {
    match error {
        ProfileError::Name => tr("Enter a profile name."),
        ProfileError::Url(error) => url_error_text(error),
        ProfileError::DeploymentNeedsDatedVersion => tr(
            "Deployment-scoped batch transcription requires a dated API version, such as \
             2025-03-01-preview.",
        ),
    }
}

/// The text for a failed request to the server of the profile `server`, in
/// the app's language, with the server's own words where it gave them.
pub fn explain(error: &PluginError, server: &str) -> String {
    match error {
        PluginError::Plugin {
            kind: Some(kind), ..
        } if kind == error_kind::NOT_CONFIGURED => {
            tr("The server profile is not complete. Enter its server URL and transcription model.")
        }
        PluginError::Plugin { .. } => crate::cloud::explain(error, server),
        other => tr_with(
            "The OpenAI-compatible plugin stopped: {error}",
            &[("error", &other.to_string())],
        ),
    }
}
