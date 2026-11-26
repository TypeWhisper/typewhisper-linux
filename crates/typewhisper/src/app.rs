//! The app: a setup window, the dictation shortcut, and the flow from
//! recording to transcription to insertion.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adw::prelude::*;
use futures_util::StreamExt;
use gtk::{gio, glib};
use serde_json::{Value, json};
use tw_platform::accessibility::{Focus, FocusedField};
use tw_platform::audio::{LevelMeter, Recorder, SAMPLE_RATE};
use tw_platform::i18n::{n_, tr, tr_with};
use tw_platform::insert::{PasteKeys, TextInserter};
use tw_platform::secrets::{SecretError, SecretStore};
use tw_platform::settings::{self, Settings};
use tw_platform::shortcuts::{self, BoundShortcut, Command, PressInterpreter};
use tw_platform::zbus;
use tw_plugin_host::{InstalledPlugin, PluginError, PluginProcess, discover, plugin_dirs};
use tw_plugin_protocol::{PluginSettings, error_kind};

use crate::APP_ID;
use crate::cloud::{self, ApiKeyGroup};
use crate::formatting;
use crate::kwin_script;
use crate::overlay::Overlay;
use crate::server_profiles::{self, ServerProfiles};
use crate::shell_bridge::ShellBridge;
use crate::terminals::{self, TerminalApps};

/// The default engine, and its name while no engine is found.
const PARAKEET: &str = "com.typewhisper.parakeet";
const PARAKEET_NAME: &str = "NVIDIA Parakeet";
const SHORTCUT_DESCRIPTION: &str = n_("Start and stop dictation");
/// Shorter recordings are treated as accidental presses.
const MIN_SAMPLES: usize = SAMPLE_RATE as usize / 4;

const MODEL_SUMMARY: &str = n_("Runs offline with no API key.");
const CLOUD_MODEL_SUMMARY: &str = n_("Runs in the cloud with your API key.");
/// The macOS plugins' setting for the model a cloud engine uses.
const SELECTED_PLUGIN_MODEL: &str = "selectedModel";
/// How long the indicator keeps the result of a dictation on screen.
const INSERTED_FEEDBACK: Duration = Duration::from_millis(900);
const ERROR_FEEDBACK: Duration = Duration::from_secs(3);
/// How often the indicator gets the input level while recording, as on macOS.
const LEVEL_INTERVAL: Duration = Duration::from_millis(33);

struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    hero_icon: gtk::Image,
    hero_spinner: adw::Spinner,
    hero_title: gtk::Label,
    hero_text: gtk::Label,
    hero_key: gtk::Label,
    hero_text_after: gtk::Label,
    typing_row: adw::ActionRow,
    typing_button: gtk::Button,
    typing_granted: gtk::Box,
    shortcut_row: adw::ActionRow,
    shortcut_button: gtk::Button,
    shortcut_key: gtk::Label,
    engine_picker: adw::ComboRow,
    engine_names: gtk::StringList,
    model_picker: adw::ComboRow,
    model_names: gtk::StringList,
    /// The model in use: its download, loading and errors.
    model_row: adw::ActionRow,
    model_button: gtk::Button,
    model_progress: gtk::Box,
    model_progress_bar: gtk::ProgressBar,
    model_progress_label: gtk::Label,
    model_spinner: adw::Spinner,
    model_ready: gtk::Box,
    api_key: ApiKeyGroup,
    last_group: adw::PreferencesGroup,
    last_row: adw::ActionRow,
    preserve_clipboard: adw::SwitchRow,
    terminal_apps: adw::PreferencesGroup,
    servers: adw::PreferencesGroup,
    app_formatting: adw::SwitchRow,
    standalone_value_period: adw::SwitchRow,
}

#[derive(Clone, Default, PartialEq)]
enum ModelState {
    /// The plugin has not answered yet.
    #[default]
    Searching,
    /// The plugin is missing or did not start.
    Unavailable(String),
    NotDownloaded,
    /// The fraction received, or `None` while the archive is unpacked.
    Downloading(Option<f64>),
    Loading,
    Ready,
    /// A cloud engine has no API key yet.
    NeedsKey,
    /// Downloading or loading failed; the download can be repeated.
    Failed(String),
    /// The plugin process died while setting up the model. The button starts a
    /// new one; there is no automatic restart, so a model that crashes the
    /// plugin on every load cannot cause a loop.
    Lost,
}

/// A model as the plugin lists it in `initialize`.
#[derive(Clone)]
struct ModelInfo {
    id: String,
    name: String,
    /// Knows only English, like Whisper's `.en` models.
    english_only: bool,
    recommended: bool,
    size_bytes: u64,
    installed: bool,
}

impl ModelInfo {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            id: value["id"].as_str()?.to_owned(),
            name: value["name"].as_str()?.to_owned(),
            english_only: value["englishOnly"].as_bool().unwrap_or(false),
            recommended: value["recommended"].as_bool().unwrap_or(false),
            size_bytes: value["sizeBytes"].as_u64().unwrap_or(0),
            installed: value["installed"].as_bool().unwrap_or(false),
        })
    }

    /// The name shown in the window.
    fn label(&self) -> String {
        if self.english_only {
            tr_with("{model} (English)", &[("model", &self.name)])
        } else {
            self.name.clone()
        }
    }
}

/// Transcription plugins, NVIDIA Parakeet first as the default, then by name.
fn engines(mut plugins: Vec<InstalledPlugin>) -> Vec<InstalledPlugin> {
    plugins.retain(|p| p.manifest.categories.iter().any(|c| c == "transcription"));
    plugins.sort_by_cached_key(|p| (p.manifest.id != PARAKEET, p.manifest.name.to_lowercase()));
    plugins
}

/// The model to use: the saved one if the engine has it, else the recommended
/// one if it is downloaded, else any downloaded one, else the recommended one.
fn pick_model(models: &[ModelInfo], saved: Option<&str>) -> Option<usize> {
    let find = |wanted: &dyn Fn(&ModelInfo) -> bool| models.iter().position(wanted);
    find(&|m| Some(m.id.as_str()) == saved)
        .or_else(|| find(&|m| m.recommended && m.installed))
        .or_else(|| find(&|m| m.installed))
        .or_else(|| find(&|m| m.recommended))
        .or_else(|| (!models.is_empty()).then_some(0))
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Phase {
    #[default]
    Idle,
    Recording,
    Transcribing,
}

#[derive(Default)]
struct State {
    connection: Option<zbus::Connection>,
    inserter: Option<Rc<TextInserter>>,
    /// Finds the focused field or terminal and reads the text around the
    /// cursor for app-aware formatting.
    focused_field: Option<Rc<FocusedField>>,
    shortcut: Option<BoundShortcut>,
    interpreter: PressInterpreter,
    recorder: Option<Recorder>,
    /// Transcription plugins found at startup, the default engine first.
    engines: Vec<InstalledPlugin>,
    /// The engine in use, an index into `engines`.
    engine: Option<usize>,
    plugin: Option<Arc<Mutex<PluginProcess>>>,
    /// The running engine's models, as it listed them when it started.
    models: Vec<ModelInfo>,
    /// The model in use, an index into `models`.
    model_index: Option<usize>,
    /// The running engine takes settings, such as an API key.
    takes_settings: bool,
    /// The desktop's keyring, opened when an engine first needs it.
    secrets: Option<Rc<SecretStore>>,
    /// True while an API key goes into the keyring, which may first ask the
    /// user to unlock it. The engine stays put meanwhile.
    saving_key: bool,
    model: ModelState,
    phase: Phase,
    busy: bool,
    /// True until the portals have answered at startup.
    starting: bool,
    /// Why the desktop portals cannot be reached.
    portal_error: Option<String>,
    /// Counts indicator updates, so that a feedback timer only hides its own state.
    indicator_generation: u64,
    bridge: Option<ShellBridge>,
    /// The indicator on desktops with a layer shell; GNOME has the extension.
    overlay: Option<Overlay>,
    terminal_apps: Option<Rc<TerminalApps>>,
    /// The profiles of the OpenAI-compatible engine, when its plugin is installed.
    server_profiles: Option<Rc<ServerProfiles>>,
    /// Profiles changed during a dictation; the plugin gets them when it ends.
    reconfigure_pending: bool,
    /// True while KWin runs the script that reports the active window.
    kwin_script: bool,
}

struct App {
    ui: Ui,
    state: RefCell<State>,
    settings: RefCell<Settings>,
    /// Set while the app itself fills the pickers, so that only the user's
    /// choices count.
    filling_pickers: Cell<bool>,
}

pub fn startup(application: &adw::Application) {
    // The shortcut keeps working after the window is closed.
    std::mem::forget(application.hold());
    let quit = gio::SimpleAction::new("quit", None);
    quit.connect_activate(glib::clone!(
        #[weak]
        application,
        move |_, _| application.quit()
    ));
    application.add_action(&quit);
    application.set_accels_for_action("app.quit", &["<Control>q"]);

    // libadwaita has no style for a key that is named in running text.
    let css = gtk::CssProvider::new();
    css.load_from_string(
        ".tw-key { padding: 2px 8px; border-radius: 6px; font-weight: 500; \
         background-color: color-mix(in srgb, var(--accent-bg-color) 18%, transparent); }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let ui = build_ui(application);
    let bridge = application
        .dbus_connection()
        .and_then(|c| ShellBridge::register(c).ok());
    let app = Rc::new(App {
        ui,
        state: RefCell::new(State {
            bridge,
            overlay: Overlay::new(),
            starting: true,
            ..Default::default()
        }),
        settings: RefCell::new(Settings::load()),
        filling_pickers: Cell::new(false),
    });
    app.refresh_model();
    app.refresh_typing_row();
    app.refresh_shortcut_row(None);

    app.ui.shortcut_button.connect_clicked(glib::clone!(
        #[strong]
        app,
        move |_| {
            glib::spawn_future_local(bind_shortcut(Rc::clone(&app)));
        }
    ));
    let about = gio::SimpleAction::new("about", None);
    about.connect_activate(glib::clone!(
        #[weak]
        app,
        move |_, _| show_about(&app.ui.window)
    ));
    application.add_action(&about);
    app.ui.typing_button.connect_clicked(glib::clone!(
        #[strong]
        app,
        move |_| {
            glib::spawn_future_local(approve_typing(Rc::clone(&app)));
        }
    ));
    app.ui.model_button.connect_clicked(glib::clone!(
        #[strong]
        app,
        move |_| {
            glib::spawn_future_local(download_model(Rc::clone(&app)));
        }
    ));
    app.ui.engine_picker.connect_selected_notify(glib::clone!(
        #[weak]
        app,
        move |picker| choose_engine(&app, picker.selected())
    ));
    app.ui.model_picker.connect_selected_notify(glib::clone!(
        #[weak]
        app,
        move |picker| choose_model(&app, picker.selected())
    ));
    app.ui.api_key.connect_save(glib::clone!(
        #[weak]
        app,
        move |key| {
            glib::spawn_future_local(save_api_key(app, Some(key)));
        }
    ));
    app.ui.api_key.connect_remove(glib::clone!(
        #[weak]
        app,
        move || {
            glib::spawn_future_local(save_api_key(app, None));
        }
    ));
    for (row, key, default) in [
        (
            &app.ui.preserve_clipboard,
            settings::PRESERVE_CLIPBOARD,
            false,
        ),
        (&app.ui.app_formatting, settings::APP_FORMATTING, true),
        (
            &app.ui.standalone_value_period,
            settings::STRIP_STANDALONE_VALUE_PERIOD,
            true,
        ),
    ] {
        bind_setting(&app, row, key, default);
    }
    let saved = app.settings.borrow().strings(settings::TERMINAL_APPS);
    let terminal_apps = TerminalApps::new(
        &app.ui.terminal_apps,
        saved,
        glib::clone!(
            #[weak]
            app,
            move |app_ids| {
                let saved = app
                    .settings
                    .borrow_mut()
                    .set_strings(settings::TERMINAL_APPS, app_ids);
                if let Err(error) = saved {
                    app.toast(&tr_with(
                        "The setting could not be saved: {error}",
                        &[("error", &error.to_string())],
                    ));
                }
            }
        ),
    );
    app.state.borrow_mut().terminal_apps = Some(terminal_apps);
    application.connect_shutdown(glib::clone!(
        #[weak]
        app,
        move |application| {
            if app.state.borrow().kwin_script
                && let Some(connection) = application.dbus_connection()
            {
                kwin_script::unload(&connection);
            }
        }
    ));
    if let Some(connection) = application.dbus_connection() {
        glib::spawn_future_local(start_kwin_script(Rc::clone(&app), connection));
    }
    glib::spawn_future_local(initialize(app));
}

/// On KDE Plasma, has KWin report the active window, so that terminals without
/// AT-SPI objects, such as Konsole, are recognized.
async fn start_kwin_script(app: Rc<App>, connection: gio::DBusConnection) {
    let Some(kwin) = kwin_script::kwin_name(&connection).await else {
        return;
    };
    // Accepted before the script starts, so its first report counts.
    match app.state.borrow().bridge.as_ref() {
        Some(bridge) => bridge.accept_kwin_reports(kwin),
        None => return,
    }
    // Set first, so that quitting while the script starts still unloads it.
    app.state.borrow_mut().kwin_script = true;
    if let Err(error) = kwin_script::load(&connection).await {
        app.state.borrow_mut().kwin_script = false;
        eprintln!("typewhisper: KWin does not report the active window: {error}");
    }
}

/// The About dialog. Like the macOS app's About panel, it names the app and
/// its version; the links and the license are the ones GNOME apps show. Legal
/// names the speech models and the libraries that run them, see issue #17.
fn show_about(window: &adw::ApplicationWindow) {
    let dialog = adw::AboutDialog::builder()
        .application_name("TypeWhisper")
        .application_icon(APP_ID)
        .version(env!("CARGO_PKG_VERSION"))
        .website("https://typewhisper.com")
        .issue_url("https://github.com/TypeWhisper/typewhisper-linux/issues")
        .license_type(gtk::License::Gpl30)
        .build();
    dialog.add_link(
        &tr("Source Code"),
        "https://github.com/TypeWhisper/typewhisper-linux",
    );
    // The speech model and the libraries the Parakeet plugin runs it with.
    // CC BY asks for credit, a link to the license and a note on changes.
    dialog.add_legal_section(
        "NVIDIA Parakeet TDT 0.6B v3",
        Some("© NVIDIA Corporation"),
        gtk::License::Custom,
        Some(&tr_with(
            "Licensed under {license}. TypeWhisper uses the int8 ONNX conversion from sherpa-onnx.",
            &[(
                "license",
                "<a href=\"https://creativecommons.org/licenses/by/4.0/\">CC BY 4.0</a>",
            )],
        )),
    );
    dialog.add_legal_section(
        "sherpa-onnx",
        Some("© Xiaomi Corporation"),
        gtk::License::Apache20,
        None,
    );
    dialog.add_legal_section(
        "ONNX Runtime",
        Some("© Microsoft Corporation"),
        gtk::License::MitX11,
        None,
    );
    // The Whisper plugin's models and the library it runs them with.
    dialog.add_legal_section(
        "OpenAI Whisper",
        Some("© OpenAI"),
        gtk::License::MitX11,
        None,
    );
    dialog.add_legal_section(
        "whisper.cpp",
        Some("© The ggml authors"),
        gtk::License::MitX11,
        None,
    );
    dialog.present(Some(window));
}

/// Shows a setting in a switch row and saves every change right away.
fn bind_setting(app: &Rc<App>, row: &adw::SwitchRow, key: &'static str, default: bool) {
    row.set_active(app.settings.borrow().bool(key, default));
    row.connect_active_notify(glib::clone!(
        #[weak]
        app,
        move |row| {
            let saved = app.settings.borrow_mut().set_bool(key, row.is_active());
            if let Err(error) = saved {
                app.toast(&tr_with(
                    "The setting could not be saved: {error}",
                    &[("error", &error.to_string())],
                ));
            }
        }
    ));
}

pub fn activate(application: &adw::Application) {
    if let Some(window) = application.windows().first() {
        window.present();
    }
}

fn build_ui(application: &adw::Application) -> Ui {
    // Titles and subtitles show text from the desktop, from errors and from
    // dictations, none of which is markup.
    let row = |title: &str, subtitle: &str, icon: &str| {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .use_markup(false)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(icon));
        row
    };
    let button = |label: &str| {
        gtk::Button::builder()
            .label(label)
            .valign(gtk::Align::Center)
            .build()
    };
    let granted = |text: &str| {
        let pill = gtk::Box::builder()
            .spacing(6)
            .valign(gtk::Align::Center)
            .css_classes(["success"])
            .visible(false)
            .build();
        pill.append(&gtk::Image::from_icon_name("object-select-symbolic"));
        pill.append(&gtk::Label::new(Some(text)));
        pill
    };
    let keycap = || {
        gtk::Label::builder()
            .css_classes(["tw-key"])
            .valign(gtk::Align::Center)
            .visible(false)
            .build()
    };

    // What the app is doing and what is still missing, in one place.
    let hero_icon = gtk::Image::builder().pixel_size(48).build();
    let hero_spinner = adw::Spinner::builder()
        .width_request(48)
        .height_request(48)
        .halign(gtk::Align::Center)
        .build();
    let hero_title = gtk::Label::builder()
        .css_classes(["title-2"])
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    let hero_label = || {
        gtk::Label::builder()
            .css_classes(["dim-label"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build()
    };
    let hero_text = hero_label();
    let hero_key = keycap();
    let hero_text_after = hero_label();
    // The line keeps its height when it is empty, so the rows below stay put.
    let hero_line = gtk::Box::builder()
        .spacing(6)
        .height_request(28)
        .halign(gtk::Align::Center)
        .build();
    hero_line.append(&hero_text);
    hero_line.append(&hero_key);
    hero_line.append(&hero_text_after);
    let hero = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(18)
        .margin_end(18)
        .build();
    hero.append(&hero_icon);
    hero.append(&hero_spinner);
    hero.append(&hero_title);
    hero.append(&hero_line);
    let hero_card = adw::Bin::builder()
        .css_classes(["card"])
        .child(&hero)
        .build();
    let status = adw::PreferencesGroup::new();
    status.add(&hero_card);

    let typing_row = row(
        &tr("Keyboard and Clipboard Access"),
        "",
        "edit-paste-symbolic",
    );
    let typing_button = button(&tr("Grant Access"));
    let typing_granted = granted(&tr("Granted"));
    typing_row.add_suffix(&typing_button);
    typing_row.add_suffix(&typing_granted);
    let permissions = adw::PreferencesGroup::builder()
        .title(tr("Permissions"))
        .build();
    permissions.add(&typing_row);

    let shortcut_row = row(&tr("Dictation Hotkey"), "", "input-keyboard-symbolic");
    let shortcut_key = keycap();
    let shortcut_button = button(&tr("Set Up"));
    shortcut_row.add_suffix(&shortcut_key);
    shortcut_row.add_suffix(&shortcut_button);
    let hotkey = adw::PreferencesGroup::builder().title(tr("Hotkey")).build();
    hotkey.add(&shortcut_row);

    // Filled once the plugins are found; the model picker shows only for an
    // engine with a choice of models, as on macOS and Windows.
    let engine_names = gtk::StringList::new(&[]);
    let engine_picker = adw::ComboRow::builder()
        .title(tr("Default Engine"))
        .model(&engine_names)
        .visible(false)
        .build();
    engine_picker.add_prefix(&gtk::Image::from_icon_name(
        "audio-input-microphone-symbolic",
    ));
    let model_names = gtk::StringList::new(&[]);
    let model_picker = adw::ComboRow::builder()
        .title(tr("Model"))
        .model(&model_names)
        .visible(false)
        .build();
    model_picker.add_prefix(&gtk::Image::from_icon_name("view-list-symbolic"));
    let model_row = row("", "", "package-x-generic-symbolic");
    let model_button = button(&tr("Download"));
    let model_progress_bar = gtk::ProgressBar::builder()
        .valign(gtk::Align::Center)
        .width_request(96)
        .build();
    let model_progress_label = gtk::Label::builder()
        .css_classes(["numeric", "dim-label"])
        .width_chars(4)
        .xalign(1.0)
        .build();
    let model_progress = gtk::Box::builder()
        .spacing(8)
        .valign(gtk::Align::Center)
        .build();
    model_progress.append(&model_progress_bar);
    model_progress.append(&model_progress_label);
    let model_spinner = adw::Spinner::builder()
        .valign(gtk::Align::Center)
        .tooltip_text(tr("Loading"))
        .build();
    let model_ready = granted(&tr("Ready"));
    model_row.add_suffix(&model_button);
    model_row.add_suffix(&model_progress);
    model_row.add_suffix(&model_spinner);
    model_row.add_suffix(&model_ready);
    let engine = adw::PreferencesGroup::builder().title(tr("Engine")).build();
    engine.add(&engine_picker);
    engine.add(&model_picker);
    engine.add(&model_row);
    // Below the engine, for an engine that needs an API key.
    let api_key = ApiKeyGroup::new();

    let last_row = adw::ActionRow::builder()
        .use_markup(false)
        .title_lines(0)
        .title_selectable(true)
        .build();
    let last_group = adw::PreferencesGroup::builder()
        .title(tr("Last Transcription"))
        .visible(false)
        .build();
    last_group.add(&last_row);

    let switch_row = |title: &str, subtitle: &str, icon: &str| {
        let row = adw::SwitchRow::builder()
            .title(title)
            .subtitle(subtitle)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(icon));
        row
    };
    let preserve_clipboard = switch_row(
        &tr("Preserve clipboard content"),
        &tr(
            "Restores your clipboard after text insertion. Without this, your clipboard \
             contains the transcribed text after dictation.",
        ),
        "edit-copy-symbolic",
    );
    let clipboard = adw::PreferencesGroup::builder()
        .title(tr("Clipboard"))
        .build();
    clipboard.add(&preserve_clipboard);

    // Filled once the settings are loaded, see `TerminalApps`.
    let terminal_apps = adw::PreferencesGroup::builder()
        .title(tr("Terminal Apps"))
        .description(tr(terminals::DESCRIPTION))
        .build();

    let app_formatting = switch_row(
        &tr("App-aware formatting"),
        &tr(
            "When enabled, TypeWhisper uses target-app rules and available cursor context \
             for smarter insertion.",
        ),
        "insert-text-symbolic",
    );
    let standalone_value_period = switch_row(
        &tr("Strip final period from standalone values"),
        &tr(
            "Removes a model-added period when a dictated email address, URL, number, or \
         version string is inserted on its own, keeping the value usable in form fields. \
         Abbreviations, dates, and sentences are left alone.",
        ),
        "format-text-plaintext-symbolic",
    );
    let formatting = adw::PreferencesGroup::builder()
        .title(tr("Output Formatting"))
        .build();
    formatting.add(&app_formatting);
    formatting.add(&standalone_value_period);

    // Filled once the plugin is found, see `ServerProfiles`.
    let servers = adw::PreferencesGroup::builder()
        .title("OpenAI Compatible")
        .description(tr(
            "Servers with OpenAI's API, such as LM Studio, Ollama, vLLM or a cloud service. \
             Each profile has its own URL, API key and models.",
        ))
        .visible(false)
        .build();

    let page = adw::PreferencesPage::new();
    page.add(&status);
    page.add(&last_group);
    page.add(&permissions);
    page.add(&hotkey);
    page.add(&engine);
    page.add(&api_key.group);
    page.add(&servers);
    page.add(&clipboard);
    page.add(&terminal_apps);
    page.add(&formatting);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&page));

    let menu = gio::Menu::new();
    menu.append(Some(&tr("About TypeWhisper")), Some("app.about"));
    menu.append(Some(&tr("Quit")), Some("app.quit"));
    let header = adw::HeaderBar::new();
    header.pack_end(
        &gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text(tr("Main Menu"))
            .primary(true)
            .menu_model(&menu)
            .build(),
    );
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&toasts));

    // The application keeps the window; activate() presents it again.
    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("TypeWhisper")
        .default_width(560)
        .default_height(700)
        .hide_on_close(true)
        .content(&view)
        .build();
    Ui {
        window,
        toasts,
        hero_icon,
        hero_spinner,
        hero_title,
        hero_text,
        hero_key,
        hero_text_after,
        typing_row,
        typing_button,
        typing_granted,
        shortcut_row,
        shortcut_button,
        shortcut_key,
        engine_picker,
        engine_names,
        model_picker,
        model_names,
        model_row,
        model_button,
        model_progress,
        model_progress_bar,
        model_progress_label,
        model_spinner,
        model_ready,
        api_key,
        last_group,
        last_row,
        preserve_clipboard,
        terminal_apps,
        servers,
        app_formatting,
        standalone_value_period,
    }
}

/// What the status card shows, translated.
struct Hero {
    /// `None` shows a spinner instead.
    icon: Option<&'static str>,
    /// Style class of the icon: `accent`, `error`, `warning` or `dim-label`.
    tone: &'static str,
    title: String,
    text: String,
    /// A key shown after `text`, followed by `text_after`.
    key: String,
    text_after: String,
}

impl Hero {
    fn new(icon: Option<&'static str>, tone: &'static str, title: String, text: String) -> Self {
        Self {
            icon,
            tone,
            title,
            text,
            key: String::new(),
            text_after: String::new(),
        }
    }
}

/// The key without the words GNOME puts around it, as in `Press Ctrl+Alt+D`.
fn key_label(trigger: &str) -> &str {
    trigger.strip_prefix("Press ").unwrap_or(trigger)
}

impl App {
    fn toast(&self, text: &str) {
        self.ui
            .toasts
            .add_toast(adw::Toast::builder().title(text).use_markup(false).build());
    }

    /// Sends a state to the indicator; `idle` hides it. The GNOME Shell
    /// extension draws it from the bus signal, the overlay on other desktops.
    fn indicator(&self, state: &str, text: &str) {
        let mut app_state = self.state.borrow_mut();
        app_state.indicator_generation += 1;
        if let Some(bridge) = app_state.bridge.as_ref() {
            bridge.set_state(state, text);
        }
        if let Some(overlay) = app_state.overlay.as_ref() {
            overlay.set_state(state, text);
        }
    }

    /// Sends the input level, from 0 to 1, to the indicator's waveform.
    fn level(&self, level: f32) {
        let state = self.state.borrow();
        if let Some(bridge) = state.bridge.as_ref() {
            bridge.set_level(level);
        }
        if let Some(overlay) = state.overlay.as_ref() {
            overlay.set_level(level);
        }
    }

    /// Shows a result in the indicator and hides it after `duration`, unless
    /// another state has replaced it by then.
    fn indicator_feedback(self: &Rc<Self>, state: &str, text: &str, duration: Duration) {
        self.indicator(state, text);
        let generation = self.state.borrow().indicator_generation;
        let app = Rc::clone(self);
        glib::timeout_add_local_once(duration, move || {
            if app.state.borrow().indicator_generation == generation {
                app.indicator("idle", "");
            }
        });
    }

    /// Reports a failed dictation where the user is looking: in the indicator
    /// and, for when the window is open, as a toast.
    fn dictation_failed(self: &Rc<Self>, text: &str) {
        self.toast(text);
        self.indicator_feedback("error", text, ERROR_FEEDBACK);
    }

    /// True when the app the desktop reports as focused is a terminal.
    fn focused_app_is_terminal(&self) -> bool {
        let Some(focused) = self.state.borrow().bridge.as_ref().map(|b| b.focused_app()) else {
            return false;
        };
        let added = self.settings.borrow().strings(settings::TERMINAL_APPS);
        terminals::is_terminal(&focused.app_id, &added)
    }

    /// The app the last text went to, when the GNOME Shell extension or the
    /// KWin script reports it.
    fn focused_target(&self) -> String {
        let focused = self
            .state
            .borrow()
            .bridge
            .as_ref()
            .map(|b| b.focused_app())
            .unwrap_or_default();
        if focused.title.is_empty() {
            focused.app_id
        } else {
            focused.title
        }
    }

    /// Drops a plugin process that died or no longer answers in order.
    fn forget_plugin(&self) {
        self.state.borrow_mut().plugin = None;
    }

    fn typing_approved(&self) -> bool {
        self.state
            .borrow()
            .inserter
            .as_ref()
            .is_some_and(|i| i.is_approved())
    }

    fn refresh_typing_row(&self) {
        let approved = self.typing_approved();
        self.ui.typing_row.set_subtitle(&if approved {
            tr("Pastes text into other apps.")
        } else {
            tr("Pastes text into other apps. In the desktop's dialog, switch on remote interaction and clipboard access.")
        });
        self.ui.typing_button.set_visible(!approved);
        self.ui.typing_granted.set_visible(approved);
        self.refresh_hero();
    }

    /// Shows the bound key, or why there is none.
    fn refresh_shortcut_row(&self, error: Option<&str>) {
        let trigger = self
            .state
            .borrow()
            .shortcut
            .as_ref()
            .map(|s| shortcuts::readable_trigger(&s.trigger_description));
        let ui = &self.ui;
        let key = trigger.as_deref().map(key_label).unwrap_or_default();
        ui.shortcut_key.set_label(key);
        ui.shortcut_key.set_visible(!key.is_empty());
        ui.shortcut_button.set_visible(trigger.is_none());
        ui.shortcut_row
            .set_subtitle(&match (error, trigger.as_deref()) {
                (Some(error), _) => tr_with("Not set: {error}", &[("error", error)]),
                (None, Some("")) => {
                    tr("No key assigned. Assign one in the desktop's keyboard settings.")
                }
                _ => tr("Short press to toggle, hold to push-to-talk."),
            });
        self.refresh_hero();
    }

    fn set_model(&self, model: ModelState) {
        self.state.borrow_mut().model = model;
        self.refresh_model();
    }

    /// The engine in use.
    fn engine(&self) -> Option<InstalledPlugin> {
        let state = self.state.borrow();
        state.engine.and_then(|i| state.engines.get(i)).cloned()
    }

    /// Who failures of the running engine are about.
    fn source(&self) -> Source {
        let engine = self.engine();
        let server = engine.as_ref().is_some_and(is_server_engine);
        let name = match self.current_model() {
            Some(model) if server => model.name,
            _ => engine_name(engine.as_ref()),
        };
        Source { name, server }
    }

    /// The server the running OpenAI-compatible engine sends recordings to.
    fn server_url(&self) -> Option<String> {
        let model = self.current_model()?;
        let profiles = self.state.borrow().server_profiles.clone()?;
        profiles.base_url(&model.id).filter(|url| !url.is_empty())
    }

    /// The model in use.
    fn current_model(&self) -> Option<ModelInfo> {
        let state = self.state.borrow();
        state.model_index.and_then(|i| state.models.get(i)).cloned()
    }

    /// Shows the engines and the running engine's models in the pickers.
    fn refresh_pickers(&self) {
        let state = self.state.borrow();
        let ui = &self.ui;
        self.filling_pickers.set(true);
        let engines: Vec<&str> = state
            .engines
            .iter()
            .map(|e| e.manifest.name.as_str())
            .collect();
        ui.engine_names
            .splice(0, ui.engine_names.n_items(), &engines);
        ui.engine_picker.set_selected(
            state
                .engine
                .map_or(gtk::INVALID_LIST_POSITION, |i| i as u32),
        );
        ui.engine_picker.set_visible(!engines.is_empty());
        let models: Vec<String> = state.models.iter().map(ModelInfo::label).collect();
        let models: Vec<&str> = models.iter().map(String::as_str).collect();
        ui.model_names.splice(0, ui.model_names.n_items(), &models);
        ui.model_picker.set_selected(
            state
                .model_index
                .map_or(gtk::INVALID_LIST_POSITION, |i| i as u32),
        );
        ui.model_picker.set_visible(models.len() > 1);
        self.filling_pickers.set(false);
    }

    /// The pickers and the API key work while nothing runs: no dictation,
    /// download, start or keyring access.
    fn refresh_picker_sensitivity(&self) {
        let state = self.state.borrow();
        let settled = matches!(
            state.model,
            ModelState::Unavailable(_)
                | ModelState::NotDownloaded
                | ModelState::Ready
                | ModelState::NeedsKey
                | ModelState::Failed(_)
                | ModelState::Lost
        );
        let sensitive = settled && state.phase == Phase::Idle && !state.busy && !state.saving_key;
        self.ui.engine_picker.set_sensitive(sensitive);
        self.ui.model_picker.set_sensitive(sensitive);
        self.ui.api_key.set_sensitive(sensitive);
    }

    fn refresh_model(&self) {
        let model = self.state.borrow().model.clone();
        let current = self.current_model();
        let ui = &self.ui;
        let several = self.state.borrow().models.len() > 1;
        let title = match (&current, self.engine()) {
            // The picker lists the models by their own names, such as "Base";
            // the row names the engine too.
            (Some(current), Some(engine)) if several => {
                format!("{} {}", engine.manifest.name, current.label())
            }
            (Some(current), _) => current.label(),
            (None, Some(engine)) => engine.manifest.name,
            (None, None) => PARAKEET_NAME.to_owned(),
        };
        ui.model_row.set_title(&title);
        let subtitle = match &model {
            ModelState::Searching => tr("Starting…"),
            ModelState::Unavailable(error) | ModelState::Failed(error) => error.clone(),
            ModelState::Lost => tr("The speech model stopped unexpectedly."),
            ModelState::NotDownloaded => format!(
                "{} {}",
                tr(MODEL_SUMMARY),
                tr_with(
                    "Connect to the internet to download it ({size}).",
                    &[(
                        "size",
                        &glib::format_size(current.map_or(0, |m| m.size_bytes)),
                    )],
                )
            ),
            ModelState::Downloading(Some(_)) => tr("Downloading…"),
            ModelState::Downloading(None) => tr("Unpacking…"),
            ModelState::Loading => tr("Loading model…"),
            ModelState::NeedsKey => tr("Enter an API key below."),
            ModelState::Ready if self.engine().is_some_and(|e| is_server_engine(&e)) => {
                // Says where the audio goes, as the profile editor does.
                tr_with(
                    "Sends recordings to {server}.",
                    &[("server", &self.server_url().unwrap_or_default())],
                )
            }
            ModelState::Ready if self.engine().is_some_and(|e| is_cloud(&e)) => {
                tr(CLOUD_MODEL_SUMMARY)
            }
            ModelState::Ready => tr(MODEL_SUMMARY),
        };
        ui.model_row.set_subtitle(&subtitle);
        ui.model_button.set_visible(matches!(
            model,
            ModelState::NotDownloaded | ModelState::Failed(_) | ModelState::Lost
        ));
        ui.model_button.set_label(&if model == ModelState::Lost {
            tr("Restart")
        } else if self.state.borrow().takes_settings {
            // A cloud engine has nothing to download; its setup is repeated.
            tr("Try Again")
        } else {
            tr("Download")
        });
        ui.model_button.set_sensitive(true);
        if let ModelState::Downloading(Some(fraction)) = model {
            ui.model_progress_bar.set_fraction(fraction);
            ui.model_progress_label
                .set_label(&format!("{:.0}%", 100.0 * fraction));
        }
        ui.model_progress
            .set_visible(matches!(model, ModelState::Downloading(Some(_))));
        ui.model_spinner.set_visible(matches!(
            model,
            ModelState::Searching | ModelState::Downloading(None) | ModelState::Loading
        ));
        ui.model_ready.set_visible(model == ModelState::Ready);
        self.refresh_picker_sensitivity();
        self.refresh_hero();
    }

    /// Shows the outcome of a failed setup step: starting, configuring,
    /// downloading or loading.
    fn setup_failed(&self, error: CallError) {
        match error {
            CallError::Failed(error) => self.set_model(ModelState::Failed(error)),
            CallError::NeedsKey => self.set_model(ModelState::NeedsKey),
            CallError::Lost => self.plugin_lost_during_setup(),
        }
    }

    /// Shows the API key group for a cloud engine, empty until the key is
    /// read from the keyring, and hides it for others.
    fn show_api_key_group(&self, engine: &InstalledPlugin) {
        let name = is_cloud(engine).then_some(engine.manifest.name.as_str());
        self.ui.api_key.show(name, None);
    }

    /// The model the running cloud engine saved, under its macOS key.
    fn plugin_model(&self) -> Option<String> {
        let engine = self.engine()?;
        let values = self.settings.borrow().plugin_values(&engine.manifest.id);
        values
            .get(model_setting(&engine))?
            .as_str()
            .map(str::to_owned)
    }

    fn save_plugin_model(&self, model: &str) {
        let Some(engine) = self.engine() else {
            return;
        };
        let saved = self.settings.borrow_mut().set_plugin_value(
            &engine.manifest.id,
            model_setting(&engine),
            json!(model),
        );
        if let Err(error) = saved {
            self.toast(&tr_with(
                "The setting could not be saved: {error}",
                &[("error", &error.to_string())],
            ));
        }
    }

    fn set_saving_key(&self, saving: bool) {
        self.state.borrow_mut().saving_key = saving;
        self.refresh_picker_sensitivity();
    }

    fn plugin_lost_during_setup(&self) {
        self.forget_plugin();
        self.set_model(ModelState::Lost);
    }

    fn set_phase(&self, phase: Phase) {
        self.state.borrow_mut().phase = phase;
        self.refresh_picker_sensitivity();
        self.refresh_hero();
    }

    /// Picks the one thing the status card says: an activity first, then the
    /// first setup step that is still open, then the invitation to dictate.
    fn refresh_hero(&self) {
        let approved = self.typing_approved();
        let state = self.state.borrow();
        let trigger = state
            .shortcut
            .as_ref()
            .map(|s| shortcuts::readable_trigger(&s.trigger_description));
        let key = trigger.as_deref().map(key_label);
        let hero = if let Some(error) = state.portal_error.as_deref() {
            Hero::new(
                Some("dialog-error-symbolic"),
                "error",
                tr("No connection to the desktop"),
                error.to_owned(),
            )
        } else if state.phase == Phase::Recording {
            Hero::new(
                Some("media-record-symbolic"),
                "error",
                tr("Recording"),
                String::new(),
            )
        } else if state.phase == Phase::Transcribing {
            Hero::new(None, "", tr("Transcribing…"), String::new())
        } else if state.starting {
            Hero::new(None, "", tr("Starting…"), String::new())
        } else if !approved {
            Hero::new(
                Some("edit-paste-symbolic"),
                "dim-label",
                tr("Needs Access"),
                tr("Access is still needed before text can be pasted into other apps."),
            )
        } else if key.is_none_or(str::is_empty) {
            Hero::new(
                Some("input-keyboard-symbolic"),
                "dim-label",
                tr("No hotkey"),
                tr("A hotkey still needs to be set before dictation can start."),
            )
        } else {
            match &state.model {
                ModelState::Searching | ModelState::Loading => {
                    Hero::new(None, "", tr("Loading model…"), String::new())
                }
                ModelState::Downloading(_) => {
                    Hero::new(None, "", tr("Downloading model…"), String::new())
                }
                ModelState::NotDownloaded => Hero::new(
                    Some("folder-download-symbolic"),
                    "dim-label",
                    tr("No model loaded"),
                    tr("A speech model still needs to be downloaded before dictation can run."),
                ),
                ModelState::Unavailable(error) | ModelState::Failed(error) => Hero::new(
                    Some("dialog-warning-symbolic"),
                    "warning",
                    tr("No model loaded"),
                    error.clone(),
                ),
                ModelState::Lost => Hero::new(
                    Some("dialog-warning-symbolic"),
                    "warning",
                    tr("No model loaded"),
                    tr("The speech model stopped unexpectedly. Restart it below."),
                ),
                ModelState::NeedsKey => Hero::new(
                    Some("dialog-password-symbolic"),
                    "dim-label",
                    tr("API key required"),
                    tr_with(
                        "An API key for {engine} still needs to be entered below before dictation can run.",
                        &[(
                            "engine",
                            &engine_name(state.engine.and_then(|i| state.engines.get(i))),
                        )],
                    ),
                ),
                ModelState::Ready => {
                    // One text, so that a translation can put the key anywhere.
                    let invitation = tr("Press {key} in any app to begin.");
                    let (before, after) = invitation
                        .split_once("{key}")
                        .unwrap_or((invitation.as_str(), ""));
                    Hero {
                        key: key.unwrap_or_default().to_owned(),
                        text_after: after.trim().to_owned(),
                        ..Hero::new(
                            Some("audio-input-microphone-symbolic"),
                            "accent",
                            tr("Ready to start dictating?"),
                            before.trim().to_owned(),
                        )
                    }
                }
            }
        };

        let ui = &self.ui;
        ui.hero_icon.set_visible(hero.icon.is_some());
        ui.hero_spinner.set_visible(hero.icon.is_none());
        if let Some(icon) = hero.icon {
            ui.hero_icon.set_icon_name(Some(icon));
            ui.hero_icon.set_css_classes(&[hero.tone]);
        }
        ui.hero_title.set_label(&hero.title);
        for (label, text) in [
            (&ui.hero_text, &hero.text),
            (&ui.hero_key, &hero.key),
            (&ui.hero_text_after, &hero.text_after),
        ] {
            label.set_label(text);
            label.set_visible(!text.is_empty());
        }
    }
}

/// Saves the server profiles in the settings and hands a running
/// OpenAI-compatible engine the change.
struct ServerHost(std::rc::Weak<App>);

impl server_profiles::Host for ServerHost {
    fn save(&self, profiles: &Value) -> std::io::Result<()> {
        let Some(app) = self.0.upgrade() else {
            return Ok(());
        };
        let mut stored = app.settings.borrow_mut();
        stored.set_plugin_value(
            tw_openai_compatible::PLUGIN_ID,
            server_profiles::PROFILES,
            profiles.clone(),
        )
    }

    fn changed(&self, _settings: &PluginSettings) {
        let Some(app) = self.0.upgrade() else {
            return;
        };
        let running = app.state.borrow().plugin.is_some();
        if running && app.engine().is_some_and(|e| is_server_engine(&e)) {
            glib::spawn_future_local(reconfigure(app));
        }
    }
}

/// Gives the running engine its changed settings and loads its model again.
async fn reconfigure(app: Rc<App>) {
    {
        // A dictation in progress keeps its server; the change follows when it ends.
        let mut state = app.state.borrow_mut();
        if state.phase != Phase::Idle || state.busy {
            state.reconfigure_pending = true;
            return;
        }
        state.reconfigure_pending = false;
        // A plugin that is starting again reads the profiles as it starts.
        if state.plugin.is_none() {
            return;
        }
    }
    // Locks the pickers until the plugin has its settings, so that its answer
    // cannot land on another engine.
    app.set_model(ModelState::Loading);
    match configure_plugin(&app).await {
        Ok(true) if app.current_model().is_some() => load_model(app).await,
        Ok(true) => app.set_model(ModelState::Unavailable(tr("No model loaded"))),
        Ok(false) => app.set_model(ModelState::NeedsKey),
        Err(error) => app.setup_failed(error),
    }
}

/// Shows the server profiles when the OpenAI-compatible plugin is among the
/// engines.
fn show_server_profiles(app: &Rc<App>) {
    let plugin = app
        .state
        .borrow()
        .engines
        .iter()
        .find(|e| is_server_engine(e))
        .cloned();
    let Some(plugin) = plugin else {
        return;
    };
    if app.state.borrow().server_profiles.is_some() {
        return;
    }
    let values = app
        .settings
        .borrow()
        .plugin_values(tw_openai_compatible::PLUGIN_ID);
    let profiles = ServerProfiles::new(
        &app.ui.servers,
        &app.ui.window,
        plugin,
        &values,
        ServerHost(Rc::downgrade(app)),
    );
    app.ui.servers.set_visible(true);
    app.state.borrow_mut().server_profiles = Some(profiles);
}

fn shortcut_flag() -> Option<std::path::PathBuf> {
    tw_platform::state_dir()
        .ok()
        .map(|d| d.join("shortcut-bound"))
}

async fn initialize(app: Rc<App>) {
    // The window shows the engines while the portals answer.
    let found = find_engines(Rc::clone(&app)).await;
    match tw_platform::portal::connection(APP_ID).await {
        Ok(connection) => {
            let token = tw_platform::state_dir()
                .map(|d| d.join("remote-desktop-token"))
                .unwrap_or_else(|_| "remote-desktop-token".into());
            glib::spawn_future_local(connect_focused_field(Rc::clone(&app), connection.clone()));
            let mut state = app.state.borrow_mut();
            state.inserter = Some(Rc::new(TextInserter::new(connection.clone(), token)));
            state.connection = Some(connection);
        }
        Err(error) => {
            {
                let mut state = app.state.borrow_mut();
                state.starting = false;
                state.portal_error = Some(error.to_string());
            }
            for button in [&app.ui.typing_button, &app.ui.shortcut_button] {
                button.set_sensitive(false);
            }
            app.refresh_hero();
            return;
        }
    }
    app.refresh_typing_row();
    if shortcut_flag().is_some_and(|f| f.exists()) {
        bind_shortcut(Rc::clone(&app)).await;
    }
    app.state.borrow_mut().starting = false;
    app.refresh_hero();
    if found {
        start_plugin(Rc::clone(&app)).await;
    }
}

/// Connects to the accessibility bus for app-aware formatting and terminals.
/// Without it, dictations are inserted unchanged, and only the app the desktop
/// reports tells a terminal apart.
async fn connect_focused_field(app: Rc<App>, session: zbus::Connection) {
    match FocusedField::connect(&session).await {
        Ok(field) => app.state.borrow_mut().focused_field = Some(Rc::new(field)),
        Err(error) => eprintln!("typewhisper: no accessibility bus: {error}"),
    }
}

async fn bind_shortcut(app: Rc<App>) {
    let Some(connection) = app.state.borrow().connection.clone() else {
        return;
    };
    app.ui.shortcut_button.set_sensitive(false);
    match shortcuts::bind(&connection, &tr(SHORTCUT_DESCRIPTION)).await {
        Ok((bound, mut events)) => {
            if let Some(flag) = shortcut_flag() {
                let _ = std::fs::write(flag, "");
            }
            app.state.borrow_mut().shortcut = Some(bound);
            app.refresh_shortcut_row(None);
            glib::spawn_future_local(glib::clone!(
                #[strong]
                app,
                async move {
                    while let Some(event) = events.next().await {
                        let command = app
                            .state
                            .borrow_mut()
                            .interpreter
                            .on_event(event, Instant::now());
                        match command {
                            Some(Command::Start) => start_recording(&app),
                            Some(Command::Stop) => stop_recording(Rc::clone(&app)),
                            None => {}
                        }
                    }
                }
            ));
        }
        Err(error) => app.refresh_shortcut_row(Some(&error.to_string())),
    }
    app.ui.shortcut_button.set_sensitive(true);
}

async fn approve_typing(app: Rc<App>) {
    let Some(inserter) = app.state.borrow().inserter.clone() else {
        return;
    };
    app.ui.typing_button.set_sensitive(false);
    if let Err(error) = inserter.approve().await {
        app.toast(&error.to_string());
    }
    app.ui.typing_button.set_sensitive(true);
    app.refresh_typing_row();
}

type PluginStart = Result<(PluginProcess, Vec<ModelInfo>), String>;

fn missing_plugin() -> String {
    if std::env::var_os("FLATPAK_ID").is_some() {
        tr_with(
            "The Parakeet add-on is not installed. Install {add_on}.",
            &[("add_on", "com.typewhisper.TypeWhisper.Plugin.Parakeet")],
        )
    } else {
        tr("The Parakeet plugin is not installed.")
    }
}

/// Finds the transcription plugins and picks the saved engine, or the default
/// one. Returns false when there is none.
async fn find_engines(app: Rc<App>) -> bool {
    let found = gio::spawn_blocking(|| -> Result<Vec<InstalledPlugin>, String> {
        let data = tw_platform::data_dir().map_err(|e| e.to_string())?;
        Ok(engines(discover(&plugin_dirs(&data.join("plugins")))))
    })
    .await
    .unwrap_or_else(|_| Err(tr("The plugin host crashed.")));
    let engines = match found {
        Ok(engines) if !engines.is_empty() => engines,
        Ok(_) => {
            app.set_model(ModelState::Unavailable(missing_plugin()));
            return false;
        }
        Err(error) => {
            app.set_model(ModelState::Unavailable(error));
            return false;
        }
    };
    // A saved engine that is no longer installed leaves the choice saved, in
    // case it comes back, and dictation uses the default engine meanwhile.
    let saved = app.settings.borrow().string(settings::SELECTED_ENGINE);
    let index = saved
        .and_then(|id| engines.iter().position(|e| e.manifest.id == id))
        .unwrap_or(0);
    {
        let mut state = app.state.borrow_mut();
        state.engines = engines;
        state.engine = Some(index);
    }
    show_server_profiles(&app);
    app.refresh_pickers();
    app.refresh_model();
    true
}

/// Starts the plugin of the engine in use and loads its model.
async fn start_plugin(app: Rc<App>) {
    let Some(engine) = app.engine() else {
        return;
    };
    {
        let mut state = app.state.borrow_mut();
        state.models.clear();
        state.model_index = None;
    }
    app.refresh_pickers();
    app.set_model(ModelState::Searching);
    app.show_api_key_group(&engine);
    let started: PluginStart = gio::spawn_blocking(move || {
        let data = tw_platform::data_dir().map_err(|e| e.to_string())?;
        let plugin_data = data.join("plugin-data").join(&engine.manifest.id);
        let (process, info) =
            PluginProcess::start(&engine, &plugin_data, env!("CARGO_PKG_VERSION"))
                .map_err(|e| e.to_string())?;
        let models = info["models"]
            .as_array()
            .map(|models| models.iter().filter_map(ModelInfo::from_json).collect())
            .unwrap_or_default();
        Ok((process, models))
    })
    .await
    .unwrap_or_else(|_| Err(tr("The plugin host crashed.")));

    match started {
        Ok((process, models)) => {
            let takes_settings = process.takes_settings();
            // A cloud engine keeps its model as its macOS plugin does.
            let saved = takes_settings
                .then(|| app.plugin_model())
                .flatten()
                .or_else(|| app.settings.borrow().string(settings::SELECTED_MODEL));
            let index = pick_model(&models, saved.as_deref());
            {
                let mut state = app.state.borrow_mut();
                state.plugin = Some(Arc::new(Mutex::new(process)));
                state.models = models;
                state.model_index = index;
                state.takes_settings = takes_settings;
            }
            app.refresh_pickers();
            if takes_settings {
                match configure_plugin(&app).await {
                    Ok(true) => {}
                    Ok(false) => return app.set_model(ModelState::NeedsKey),
                    Err(error) => return app.setup_failed(error),
                }
            }
            // The settings may have brought the models, as for server profiles.
            let (index, installed) = {
                let state = app.state.borrow();
                let index = state.model_index;
                (index, index.is_some_and(|i| state.models[i].installed))
            };
            match index {
                Some(_) if installed => load_model(app).await,
                Some(_) => app.set_model(ModelState::NotDownloaded),
                None => app.set_model(ModelState::Unavailable(tr("No model loaded"))),
            }
        }
        Err(error) => app.set_model(ModelState::Unavailable(error)),
    }
}

/// Saves a setting the user chose; a failure shows as a toast.
fn save_choice(app: &App, key: &str, value: &str) {
    let saved = app.settings.borrow_mut().set_string(key, value);
    if let Err(error) = saved {
        app.toast(&tr_with(
            "The setting could not be saved: {error}",
            &[("error", &error.to_string())],
        ));
    }
}

/// Switches to the engine at `index` of the picker: stops the running plugin
/// and starts the new one.
fn choose_engine(app: &Rc<App>, index: u32) {
    if app.filling_pickers.get() {
        return;
    }
    let index = index as usize;
    let (id, old) = {
        let mut state = app.state.borrow_mut();
        if state.engine == Some(index) {
            return;
        }
        let Some(engine) = state.engines.get(index) else {
            return;
        };
        let id = engine.manifest.id.clone();
        state.engine = Some(index);
        (id, state.plugin.take())
    };
    save_choice(app, settings::SELECTED_ENGINE, &id);
    // Refuses dictations until the new engine is ready.
    app.set_model(ModelState::Searching);
    // Stopping the old plugin waits for it to exit, so it happens off the
    // main thread.
    if let Some(old) = old {
        drop(gio::spawn_blocking(move || drop(old)));
    }
    glib::spawn_future_local(start_plugin(Rc::clone(app)));
}

/// Switches to the model at `index` of the picker. A downloaded model loads
/// right away; another one waits for the download.
fn choose_model(app: &Rc<App>, index: u32) {
    if app.filling_pickers.get() {
        return;
    }
    let index = index as usize;
    let model = {
        let mut state = app.state.borrow_mut();
        if state.model_index == Some(index) {
            return;
        }
        let Some(model) = state.models.get(index).cloned() else {
            return;
        };
        state.model_index = Some(index);
        model
    };
    save_choice(app, settings::SELECTED_MODEL, &model.id);
    if app.state.borrow().takes_settings {
        app.save_plugin_model(&model.id);
    }
    if model.installed {
        glib::spawn_future_local(load_model(Rc::clone(app)));
    } else {
        app.set_model(ModelState::NotDownloaded);
    }
}

/// A cloud engine sends the audio to a service, so the window names where it
/// goes and asks for the key there.
fn is_cloud(engine: &InstalledPlugin) -> bool {
    engine.manifest.locality == "cloud"
}

/// The engine's name, for texts that name it.
fn engine_name(engine: Option<&InstalledPlugin>) -> String {
    engine.map(|e| e.manifest.name.clone()).unwrap_or_default()
}

/// The OpenAI-compatible engine, whose models are the user's server profiles.
fn is_server_engine(engine: &InstalledPlugin) -> bool {
    engine.manifest.id == tw_openai_compatible::PLUGIN_ID
}

/// The plugin setting that keeps the engine's model. For the OpenAI-compatible
/// engine it is a profile, which the macOS plugin keeps elsewhere; its
/// `selectedModel` means the default profile's transcription model there.
fn model_setting(engine: &InstalledPlugin) -> &'static str {
    if is_server_engine(engine) {
        tw_openai_compatible::SELECTED_PROFILE
    } else {
        SELECTED_PLUGIN_MODEL
    }
}

/// Who a failure is about, for the texts that explain it.
#[derive(Clone)]
struct Source {
    /// The engine's name, or the server profile's for the OpenAI-compatible engine.
    name: String,
    /// The OpenAI-compatible engine: settings it misses belong in a profile,
    /// not in an API key field.
    server: bool,
}

fn keyring_failed(error: &SecretError) -> CallError {
    CallError::Failed(tr_with(
        "The keyring could not be read: {error}",
        &[("error", &error.to_string())],
    ))
}

/// The desktop's keyring, opened on first use. Opening it may ask the user
/// to unlock it.
async fn secret_store(app: &App) -> Result<Rc<SecretStore>, SecretError> {
    if let Some(store) = app.state.borrow().secrets.clone() {
        return Ok(store);
    }
    let store = Rc::new(SecretStore::open().await?);
    app.state.borrow_mut().secrets = Some(Rc::clone(&store));
    Ok(store)
}

/// Gives the running engine its settings and secrets. Returns whether the
/// plugin has what it needs, as it answers.
async fn configure_plugin(app: &Rc<App>) -> Result<bool, CallError> {
    let (Some(engine), Some(plugin)) = (app.engine(), app.state.borrow().plugin.clone()) else {
        return Ok(false);
    };
    let id = engine.manifest.id.clone();
    let server_profiles = app.state.borrow().server_profiles.clone();
    // Only a cloud engine opens the keyring, so a local one never asks to unlock it.
    let mut secrets = std::collections::BTreeMap::new();
    if let Some(profiles) = server_profiles.filter(|_| is_server_engine(&engine)) {
        // The profiles keep their own keys, one for each server.
        profiles.load_keys().await;
        secrets = profiles.plugin_settings().secrets;
    } else if is_cloud(&engine) {
        let store = secret_store(app).await.map_err(|e| keyring_failed(&e))?;
        secrets = store.load_all(&id).await.map_err(|e| keyring_failed(&e))?;
        app.ui.api_key.show(
            Some(&engine.manifest.name),
            secrets.get(cloud::API_KEY).map(String::as_str),
        );
    }
    let settings = PluginSettings {
        values: app.settings.borrow().plugin_values(&id),
        secrets,
    };
    let source = app.source();
    let answer = gio::spawn_blocking(move || -> Result<Value, CallError> {
        let mut process = plugin.lock().map_err(|_| CallError::Lost)?;
        process
            .configure(&settings)
            .map_err(|e| CallError::from_plugin(e, &source))
    })
    .await
    .unwrap_or(Err(CallError::Lost))?;
    if let Some(models) = answer["models"].as_array() {
        let models: Vec<ModelInfo> = models.iter().filter_map(ModelInfo::from_json).collect();
        let saved = app.plugin_model();
        let index = pick_model(&models, saved.as_deref());
        {
            let mut state = app.state.borrow_mut();
            state.models = models;
            state.model_index = index;
        }
        app.refresh_pickers();
    }
    Ok(answer["configured"].as_bool().unwrap_or(true))
}

/// Saves the API key of the engine in use, or removes it with `None`, and
/// hands the running plugin the change.
async fn save_api_key(app: Rc<App>, key: Option<String>) {
    let Some(engine) = app.engine() else {
        return;
    };
    let key = key.filter(|k| !k.is_empty());
    app.set_saving_key(true);
    let saved = match secret_store(&app).await {
        Ok(store) => match &key {
            Some(key) => store.store(&engine.manifest.id, cloud::API_KEY, key).await,
            None => store.delete(&engine.manifest.id, cloud::API_KEY).await,
        },
        Err(error) => Err(error),
    };
    app.set_saving_key(false);
    // The pickers were locked, but the engine must still be the one whose key
    // was saved before its plugin hears of it.
    if app.engine().map(|e| e.manifest.id) != Some(engine.manifest.id.clone()) {
        return;
    }
    if let Err(error) = saved {
        app.toast(&tr_with(
            "The API key could not be saved in the keyring: {error}",
            &[("error", &error.to_string())],
        ));
        return;
    }
    app.ui.api_key.show_key(key.as_deref());
    app.toast(&if key.is_some() {
        tr("API key saved.")
    } else {
        tr("API key removed.")
    });
    if app.state.borrow().plugin.is_none() {
        start_plugin(app).await;
        return;
    }
    // Locks the pickers until the plugin has the key, as in `reconfigure`.
    app.set_model(ModelState::Loading);
    match configure_plugin(&app).await {
        Ok(true) => load_model(app).await,
        Ok(false) => app.set_model(ModelState::NeedsKey),
        Err(error) => app.setup_failed(error),
    }
}

fn call_plugin(
    plugin: &Arc<Mutex<PluginProcess>>,
    source: &Source,
    method: &str,
    params: Value,
) -> Result<Value, CallError> {
    let mut process = plugin.lock().map_err(|_| CallError::Lost)?;
    process
        .call(method, params, &mut |_, _| {})
        .map_err(|e| CallError::from_plugin(e, source))
}

async fn load_model(app: Rc<App>) {
    let Some(plugin) = app.state.borrow().plugin.clone() else {
        return;
    };
    let Some(model) = app.current_model() else {
        return;
    };
    app.set_model(ModelState::Loading);
    let source = app.source();
    let loaded = gio::spawn_blocking(move || {
        call_plugin(
            &plugin,
            &source,
            "transcription.loadModel",
            json!({"modelId": model.id, "threads": 4}),
        )
    })
    .await
    .unwrap_or(Err(CallError::Lost));
    match loaded {
        Ok(_) => app.set_model(ModelState::Ready),
        Err(error) => app.setup_failed(error),
    }
}

async fn download_model(app: Rc<App>) {
    let plugin = app.state.borrow().plugin.clone();
    let Some(plugin) = plugin else {
        // The process died earlier; the button now restarts it. It stays
        // disabled meanwhile, so a second click cannot start another process.
        app.ui.model_button.set_sensitive(false);
        start_plugin(Rc::clone(&app)).await;
        if app.state.borrow().plugin.is_none() {
            app.ui.model_button.set_sensitive(true);
        }
        return;
    };
    let Some(model) = app.current_model() else {
        return;
    };
    if app.state.borrow().takes_settings {
        // A cloud engine fails mostly at its settings, for example when the
        // keyring stayed locked, so trying again starts there.
        app.set_model(ModelState::Loading);
        match configure_plugin(&app).await {
            Ok(true) => {}
            Ok(false) => return app.set_model(ModelState::NeedsKey),
            Err(error) => return app.setup_failed(error),
        }
        if model.installed {
            return load_model(app).await;
        }
    }
    app.set_model(ModelState::Downloading(Some(0.0)));
    let source = app.source();
    let (progress_tx, progress_rx) = async_channel::unbounded::<Option<f64>>();
    glib::spawn_future_local(glib::clone!(
        #[strong]
        app,
        async move {
            while let Ok(progress) = progress_rx.recv().await {
                app.set_model(ModelState::Downloading(progress));
            }
        }
    ));
    let result = gio::spawn_blocking(move || -> Result<Value, CallError> {
        let mut process = plugin.lock().map_err(|_| CallError::Lost)?;
        process
            .call(
                "models.download",
                json!({"modelId": model.id, "force": true}),
                &mut |method, params| {
                    if method != "models.progress" {
                        return;
                    }
                    let progress = if params["unpacking"].as_bool() == Some(true) {
                        None
                    } else {
                        let received = params["receivedBytes"].as_f64().unwrap_or(0.0);
                        let total = params["totalBytes"].as_f64().unwrap_or(1.0).max(1.0);
                        Some((received / total).clamp(0.0, 1.0))
                    };
                    let _ = progress_tx.send_blocking(progress);
                },
            )
            .map_err(|e| CallError::from_plugin(e, &source))
    })
    .await
    .unwrap_or(Err(CallError::Lost));
    match result {
        Ok(_) => {
            {
                let mut state = app.state.borrow_mut();
                if let Some(index) = state.model_index {
                    state.models[index].installed = true;
                }
            }
            load_model(app).await;
        }
        Err(error) => app.setup_failed(error),
    }
}

fn start_recording(app: &Rc<App>) {
    let mut state = app.state.borrow_mut();
    let refusal = if state.busy {
        Some(tr("Still working on the last dictation."))
    } else {
        match state.model {
            ModelState::Ready => None,
            ModelState::Searching | ModelState::Loading => Some(tr("Loading model…")),
            ModelState::NeedsKey => Some(tr("An API key is required for transcription.")),
            _ => Some(tr("No model loaded")),
        }
    };
    if let Some(text) = refusal {
        let busy = state.busy;
        state.interpreter.reset();
        drop(state);
        if busy {
            // The indicator already shows the running dictation. Error feedback
            // would replace it and hide it before the text is inserted.
            app.toast(&text);
        } else {
            app.dictation_failed(&text);
        }
        return;
    }
    match Recorder::start() {
        Ok(recorder) => {
            let meter = recorder.level_meter();
            state.recorder = Some(recorder);
            drop(state);
            app.set_phase(Phase::Recording);
            app.indicator("recording", &tr("Recording"));
            report_levels(app, meter);
        }
        Err(error) => {
            state.interpreter.reset();
            drop(state);
            app.dictation_failed(&error);
        }
    }
}

/// Passes the input level to the indicator while the recording runs.
fn report_levels(app: &Rc<App>, meter: LevelMeter) {
    let app = Rc::downgrade(app);
    glib::timeout_add_local(LEVEL_INTERVAL, move || {
        let Some(app) = app.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if app.state.borrow().phase != Phase::Recording {
            return glib::ControlFlow::Break;
        }
        app.level(meter.level());
        glib::ControlFlow::Continue
    });
}

fn stop_recording(app: Rc<App>) {
    let (recorder, plugin, inserter) = {
        let mut state = app.state.borrow_mut();
        let Some(recorder) = state.recorder.take() else {
            return;
        };
        state.busy = true;
        (recorder, state.plugin.clone(), state.inserter.clone())
    };
    app.set_phase(Phase::Transcribing);
    app.indicator("processing", &tr("Transcribing…"));
    let source = app.source();
    glib::spawn_future_local(async move {
        let transcribed = gio::spawn_blocking(move || -> Result<String, CallError> {
            let samples = recorder.stop().map_err(CallError::Failed)?;
            if samples.len() < MIN_SAMPLES {
                return Ok(String::new());
            }
            let plugin =
                plugin.ok_or_else(|| CallError::Failed(tr("The speech model is not loaded.")))?;
            let mut process = plugin.lock().map_err(|_| CallError::Lost)?;
            let result = process
                .transcribe(&samples, None)
                .map_err(|e| CallError::from_plugin(e, &source))?;
            Ok(result["text"].as_str().unwrap_or_default().to_owned())
        })
        .await
        .unwrap_or(Err(CallError::Lost));

        // What the indicator ends on: `Ok` for inserted text, `Err` with the reason otherwise.
        let outcome = match transcribed {
            Ok(text) if text.is_empty() => Err(tr("No speech detected")),
            Ok(text) => {
                app.ui.last_row.set_title(&text);
                app.ui.last_row.set_subtitle("");
                app.ui.last_group.set_visible(true);
                match inserter {
                    Some(inserter) => {
                        let (preserve, app_formatting, standalone_value_period) = {
                            let stored = app.settings.borrow();
                            (
                                stored.bool(settings::PRESERVE_CLIPBOARD, false),
                                stored.bool(settings::APP_FORMATTING, true),
                                stored.bool(settings::STRIP_STANDALONE_VALUE_PERIOD, true),
                            )
                        };
                        // While TypeWhisper's own window is active, the paste goes there.
                        let own_window = app.ui.window.is_active();
                        let field = app.state.borrow().focused_field.clone();
                        let focus = match field {
                            Some(field) if !own_window => field.focus(app_formatting).await,
                            _ => Focus::Unknown,
                        };
                        let terminal = match &focus {
                            Focus::Terminal => true,
                            Focus::Field(_) => false,
                            // The app shows nothing over AT-SPI: ask the desktop.
                            Focus::Unknown => !own_window && app.focused_app_is_terminal(),
                        };
                        let context = match focus {
                            Focus::Field(context) => context,
                            Focus::Terminal | Focus::Unknown => None,
                        };
                        let formatted = formatting::text_for_insertion(
                            &text,
                            context.as_ref(),
                            app_formatting,
                            standalone_value_period,
                        );
                        let keys = if terminal {
                            PasteKeys::ControlShiftV
                        } else {
                            PasteKeys::ControlV
                        };
                        let inserted = inserter.insert(&formatted, preserve, keys).await;
                        app.refresh_typing_row();
                        match inserted {
                            Ok(()) => {
                                app.ui.last_row.set_subtitle(&app.focused_target());
                                Ok(())
                            }
                            Err(error) => Err(error.to_string()),
                        }
                    }
                    None => Err(tr("No connection to the desktop portals.")),
                }
            }
            Err(CallError::Failed(error)) => Err(error),
            Err(CallError::NeedsKey) => {
                app.set_model(ModelState::NeedsKey);
                Err(tr("An API key is required for transcription."))
            }
            Err(CallError::Lost) => {
                // The process is gone or in an unknown state. Start a new one,
                // which loads the model again.
                app.forget_plugin();
                app.set_model(ModelState::Searching);
                glib::spawn_future_local(start_plugin(Rc::clone(&app)));
                Err(tr("The speech model stopped. Starting it again…"))
            }
        };
        app.state.borrow_mut().busy = false;
        app.set_phase(Phase::Idle);
        if std::mem::take(&mut app.state.borrow_mut().reconfigure_pending) {
            glib::spawn_future_local(reconfigure(Rc::clone(&app)));
        }
        match outcome {
            Ok(()) => app.indicator_feedback("inserting", &tr("Inserted"), INSERTED_FEEDBACK),
            Err(error) => app.dictation_failed(&error),
        }
    });
}

enum CallError {
    /// The plugin or the recording reported an error; the process is fine.
    Failed(String),
    /// A cloud engine has no API key.
    NeedsKey,
    /// The plugin process died, stopped answering, panicked a worker or no
    /// longer answers in order.
    Lost,
}

impl CallError {
    /// Sorts a plugin error; a failure the service reports is explained in
    /// the app's words with the engine's name.
    fn from_plugin(error: PluginError, source: &Source) -> Self {
        match error {
            PluginError::Io(_) | PluginError::Protocol(_) | PluginError::TimedOut(_) => {
                CallError::Lost
            }
            other if source.server => {
                CallError::Failed(server_profiles::explain(&other, &source.name))
            }
            PluginError::Plugin {
                kind: Some(ref kind),
                ..
            } if kind == error_kind::NOT_CONFIGURED => CallError::NeedsKey,
            other => CallError::Failed(cloud::explain(&other, &source.name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(id: &str, name: &str, category: &str) -> InstalledPlugin {
        let manifest = json!({
            "id": id, "name": name, "version": "0.1.0", "protocolVersion": 1,
            "minHostVersion": "0.1.0", "executable": "bin/plugin", "categories": [category],
            "locality": "local", "architectures": ["x64", "arm64"]
        });
        InstalledPlugin {
            manifest: serde_json::from_value(manifest).unwrap(),
            dir: id.into(),
            is_system: false,
        }
    }

    #[test]
    fn lists_parakeet_first_and_only_transcription_engines() {
        let found = engines(vec![
            plugin("com.typewhisper.whisper-cpp", "Whisper", "transcription"),
            plugin("com.example.formatter", "Formatter", "llm"),
            plugin(PARAKEET, "NVIDIA Parakeet", "transcription"),
            plugin("com.example.another", "Another", "transcription"),
        ]);
        let ids: Vec<&str> = found.iter().map(|p| p.manifest.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                PARAKEET,
                "com.example.another",
                "com.typewhisper.whisper-cpp"
            ]
        );
    }

    fn model(id: &str, recommended: bool, installed: bool) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: id.into(),
            english_only: false,
            recommended,
            size_bytes: 1,
            installed,
        }
    }

    #[test]
    fn picks_the_saved_then_a_downloaded_then_the_recommended_model() {
        let models = [
            model("tiny", false, true),
            model("base", true, false),
            model("small", false, true),
        ];
        assert_eq!(pick_model(&models, Some("small")), Some(2));
        // A model of another engine, or none saved.
        assert_eq!(pick_model(&models, Some("parakeet")), Some(0));
        assert_eq!(pick_model(&models, None), Some(0));

        let fresh = [model("tiny", false, false), model("base", true, false)];
        assert_eq!(pick_model(&fresh, None), Some(1));
        let ready = [model("tiny", false, true), model("base", true, true)];
        assert_eq!(pick_model(&ready, None), Some(1));
        assert_eq!(pick_model(&[model("only", false, false)], None), Some(0));
        assert_eq!(pick_model(&[], Some("base")), None);
    }

    #[test]
    fn reads_the_models_of_both_plugins() {
        let whisper = ModelInfo::from_json(&json!({
            "id": "base.en", "name": "Base", "englishOnly": true,
            "recommended": false, "sizeBytes": 147_964_211, "installed": false
        }))
        .unwrap();
        assert!(whisper.english_only);
        assert_eq!(whisper.label(), "Base (English)");
        // Parakeet's answer has no flags.
        let parakeet = ModelInfo::from_json(&json!({
            "id": "parakeet-tdt-0.6b-v3-int8", "name": "NVIDIA Parakeet TDT 0.6B v3",
            "languages": "25 European languages", "sizeBytes": 487_211_443, "installed": true
        }))
        .unwrap();
        assert!(parakeet.installed && !parakeet.english_only && !parakeet.recommended);
        assert!(ModelInfo::from_json(&json!({"id": "x"})).is_none());
    }
}
