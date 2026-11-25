//! The app: a setup window, the dictation shortcut, and the flow from
//! recording to transcription to insertion.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adw::prelude::*;
use futures_util::StreamExt;
use gtk::{gio, glib};
use serde_json::{Value, json};
use tw_platform::accessibility::{Focus, FocusedField};
use tw_platform::audio::{Recorder, SAMPLE_RATE};
use tw_platform::i18n::{n_, tr, tr_with};
use tw_platform::insert::{PasteKeys, TextInserter};
use tw_platform::settings::{self, Settings};
use tw_platform::shortcuts::{self, BoundShortcut, Command, PressInterpreter};
use tw_platform::zbus;
use tw_plugin_host::{PluginError, PluginProcess, discover, plugin_dirs};

use crate::APP_ID;
use crate::formatting;
use crate::kwin_script;
use crate::overlay::Overlay;
use crate::shell_bridge::ShellBridge;
use crate::terminals::{self, TerminalApps};

const PARAKEET: &str = "com.typewhisper.parakeet";
const SHORTCUT_DESCRIPTION: &str = n_("Start and stop dictation");
/// Shorter recordings are treated as accidental presses.
const MIN_SAMPLES: usize = SAMPLE_RATE as usize / 4;

/// The model the Parakeet plugin ships.
const MODEL_NAME: &str = "NVIDIA Parakeet TDT 0.6B v3";
const MODEL_SUMMARY: &str = n_("Runs offline with no API key.");
/// How long the indicator keeps the result of a dictation on screen.
const INSERTED_FEEDBACK: Duration = Duration::from_millis(900);
const ERROR_FEEDBACK: Duration = Duration::from_secs(3);

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
    model_row: adw::ActionRow,
    model_button: gtk::Button,
    model_progress: gtk::Box,
    model_progress_bar: gtk::ProgressBar,
    model_progress_label: gtk::Label,
    model_spinner: adw::Spinner,
    model_ready: gtk::Box,
    last_group: adw::PreferencesGroup,
    last_row: adw::ActionRow,
    preserve_clipboard: adw::SwitchRow,
    terminal_apps: adw::PreferencesGroup,
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
    /// Downloading or loading failed; the download can be repeated.
    Failed(String),
    /// The plugin process died while setting up the model. The button starts a
    /// new one; there is no automatic restart, so a model that crashes the
    /// plugin on every load cannot cause a loop.
    Lost,
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
    plugin: Option<Arc<Mutex<PluginProcess>>>,
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
    /// True while KWin runs the script that reports the active window.
    kwin_script: bool,
}

struct App {
    ui: Ui,
    state: RefCell<State>,
    settings: RefCell<Settings>,
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
/// its version; the links and the license are the ones GNOME apps show.
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

    let model_row = row(MODEL_NAME, "", "audio-input-microphone-symbolic");
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
    engine.add(&model_row);

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

    let page = adw::PreferencesPage::new();
    page.add(&status);
    page.add(&last_group);
    page.add(&permissions);
    page.add(&hotkey);
    page.add(&engine);
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
        model_row,
        model_button,
        model_progress,
        model_progress_bar,
        model_progress_label,
        model_spinner,
        model_ready,
        last_group,
        last_row,
        preserve_clipboard,
        terminal_apps,
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

    fn refresh_model(&self) {
        let model = self.state.borrow().model.clone();
        let ui = &self.ui;
        let subtitle = match &model {
            ModelState::Searching => tr("Looking for the Parakeet plugin…"),
            ModelState::Unavailable(error) | ModelState::Failed(error) => error.clone(),
            ModelState::Lost => tr("The speech model stopped unexpectedly."),
            ModelState::NotDownloaded => format!(
                "{} {}",
                tr(MODEL_SUMMARY),
                tr("Connect to the internet to download it (465 MB).")
            ),
            ModelState::Downloading(Some(_)) => tr("Downloading…"),
            ModelState::Downloading(None) => tr("Unpacking…"),
            ModelState::Loading => tr("Loading model…"),
            ModelState::Ready => tr(MODEL_SUMMARY),
        };
        ui.model_row.set_subtitle(&subtitle);
        ui.model_button.set_visible(matches!(
            model,
            ModelState::NotDownloaded | ModelState::Failed(_) | ModelState::Lost
        ));
        ui.model_button.set_label(&if model == ModelState::Lost {
            tr("Restart")
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
        self.refresh_hero();
    }

    fn plugin_lost_during_setup(&self) {
        self.forget_plugin();
        self.set_model(ModelState::Lost);
    }

    fn set_phase(&self, phase: Phase) {
        self.state.borrow_mut().phase = phase;
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

fn shortcut_flag() -> Option<std::path::PathBuf> {
    tw_platform::state_dir()
        .ok()
        .map(|d| d.join("shortcut-bound"))
}

async fn initialize(app: Rc<App>) {
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
    start_plugin(Rc::clone(&app)).await;
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

type PluginStart = Result<(PluginProcess, bool), String>;

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

async fn start_plugin(app: Rc<App>) {
    app.set_model(ModelState::Searching);
    let started: PluginStart = gio::spawn_blocking(|| {
        let data = tw_platform::data_dir().map_err(|e| e.to_string())?;
        let plugins = discover(&plugin_dirs(&data.join("plugins")));
        let parakeet = plugins
            .iter()
            .find(|p| p.manifest.id == PARAKEET)
            .ok_or_else(missing_plugin)?;
        let plugin_data = data.join("plugin-data").join(PARAKEET);
        let (process, info) =
            PluginProcess::start(parakeet, &plugin_data, env!("CARGO_PKG_VERSION"))
                .map_err(|e| e.to_string())?;
        let installed = info["models"][0]["installed"].as_bool().unwrap_or(false);
        Ok((process, installed))
    })
    .await
    .unwrap_or_else(|_| Err(tr("The plugin host crashed.")));

    match started {
        Ok((process, installed)) => {
            app.state.borrow_mut().plugin = Some(Arc::new(Mutex::new(process)));
            if installed {
                load_model(app).await;
            } else {
                app.set_model(ModelState::NotDownloaded);
            }
        }
        Err(error) => app.set_model(ModelState::Unavailable(error)),
    }
}

fn call_plugin(
    plugin: &Arc<Mutex<PluginProcess>>,
    method: &str,
    params: Value,
) -> Result<Value, CallError> {
    let mut process = plugin.lock().map_err(|_| CallError::Lost)?;
    Ok(process.call(method, params, &mut |_, _| {})?)
}

async fn load_model(app: Rc<App>) {
    let Some(plugin) = app.state.borrow().plugin.clone() else {
        return;
    };
    app.set_model(ModelState::Loading);
    let loaded = gio::spawn_blocking(move || {
        call_plugin(&plugin, "transcription.loadModel", json!({"threads": 4}))
    })
    .await
    .unwrap_or(Err(CallError::Lost));
    match loaded {
        Ok(_) => app.set_model(ModelState::Ready),
        Err(CallError::Failed(error)) => app.set_model(ModelState::Failed(error)),
        Err(CallError::Lost) => app.plugin_lost_during_setup(),
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
    app.set_model(ModelState::Downloading(Some(0.0)));
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
        Ok(process.call(
            "models.download",
            json!({"force": true}),
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
        )?)
    })
    .await
    .unwrap_or(Err(CallError::Lost));
    match result {
        Ok(_) => load_model(app).await,
        Err(CallError::Failed(error)) => app.set_model(ModelState::Failed(error)),
        Err(CallError::Lost) => app.plugin_lost_during_setup(),
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
            state.recorder = Some(recorder);
            drop(state);
            app.set_phase(Phase::Recording);
            app.indicator("recording", &tr("Recording"));
        }
        Err(error) => {
            state.interpreter.reset();
            drop(state);
            app.dictation_failed(&error);
        }
    }
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
    glib::spawn_future_local(async move {
        let transcribed = gio::spawn_blocking(move || -> Result<String, CallError> {
            let samples = recorder.stop().map_err(CallError::Failed)?;
            if samples.len() < MIN_SAMPLES {
                return Ok(String::new());
            }
            let plugin =
                plugin.ok_or_else(|| CallError::Failed(tr("The speech model is not loaded.")))?;
            let mut process = plugin.lock().map_err(|_| CallError::Lost)?;
            let result = process.transcribe(&samples, None)?;
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
        match outcome {
            Ok(()) => app.indicator_feedback("inserting", &tr("Inserted"), INSERTED_FEEDBACK),
            Err(error) => app.dictation_failed(&error),
        }
    });
}

enum CallError {
    /// The plugin or the recording reported an error; the process is fine.
    Failed(String),
    /// The plugin process died, stopped answering, panicked a worker or no
    /// longer answers in order.
    Lost,
}

impl From<PluginError> for CallError {
    fn from(error: PluginError) -> Self {
        match error {
            PluginError::Io(_) | PluginError::Protocol(_) | PluginError::TimedOut(_) => {
                CallError::Lost
            }
            other => CallError::Failed(other.to_string()),
        }
    }
}
