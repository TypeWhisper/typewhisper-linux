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
use tw_platform::audio::{Recorder, SAMPLE_RATE};
use tw_platform::insert::TextInserter;
use tw_platform::shortcuts::{self, BoundShortcut, Command, PressInterpreter};
use tw_platform::zbus;
use tw_plugin_host::{PluginError, PluginProcess, discover, plugin_dirs};

use crate::APP_ID;
use crate::shell_bridge::ShellBridge;

const PARAKEET: &str = "com.typewhisper.parakeet";
const SHORTCUT_DESCRIPTION: &str = "Start and stop dictation";
/// Shorter recordings are treated as accidental presses.
const MIN_SAMPLES: usize = SAMPLE_RATE as usize / 4;

/// The model the Parakeet plugin ships.
const MODEL_NAME: &str = "NVIDIA Parakeet TDT 0.6B v3";
const MODEL_SUMMARY: &str = "Runs offline with no API key.";
/// How long the indicator keeps the result of a dictation on screen.
const INSERTED_FEEDBACK: Duration = Duration::from_millis(900);
const ERROR_FEEDBACK: Duration = Duration::from_secs(3);

struct Ui {
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
}

struct App {
    ui: Ui,
    state: RefCell<State>,
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
            starting: true,
            ..Default::default()
        }),
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
    glib::spawn_future_local(initialize(app));
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

    let typing_row = row("Keyboard and Clipboard Access", "", "edit-paste-symbolic");
    let typing_button = button("Grant Access");
    let typing_granted = granted("Granted");
    typing_row.add_suffix(&typing_button);
    typing_row.add_suffix(&typing_granted);
    let permissions = adw::PreferencesGroup::builder()
        .title("Permissions")
        .build();
    permissions.add(&typing_row);

    let shortcut_row = row("Dictation Hotkey", "", "input-keyboard-symbolic");
    let shortcut_key = keycap();
    let shortcut_button = button("Set Up");
    shortcut_row.add_suffix(&shortcut_key);
    shortcut_row.add_suffix(&shortcut_button);
    let hotkey = adw::PreferencesGroup::builder().title("Hotkey").build();
    hotkey.add(&shortcut_row);

    let model_row = row(MODEL_NAME, "", "audio-input-microphone-symbolic");
    let model_button = button("Download");
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
        .tooltip_text("Loading")
        .build();
    let model_ready = granted("Ready");
    model_row.add_suffix(&model_button);
    model_row.add_suffix(&model_progress);
    model_row.add_suffix(&model_spinner);
    model_row.add_suffix(&model_ready);
    let engine = adw::PreferencesGroup::builder().title("Engine").build();
    engine.add(&model_row);

    let last_row = adw::ActionRow::builder()
        .use_markup(false)
        .title_lines(0)
        .title_selectable(true)
        .build();
    let last_group = adw::PreferencesGroup::builder()
        .title("Last Transcription")
        .visible(false)
        .build();
    last_group.add(&last_row);

    let page = adw::PreferencesPage::new();
    page.add(&status);
    page.add(&last_group);
    page.add(&permissions);
    page.add(&hotkey);
    page.add(&engine);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&page));

    let menu = gio::Menu::new();
    menu.append(Some("Quit"), Some("app.quit"));
    let header = adw::HeaderBar::new();
    header.pack_end(
        &gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Main Menu")
            .primary(true)
            .menu_model(&menu)
            .build(),
    );
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&toasts));

    // The application keeps the window; activate() presents it again.
    let _window = adw::ApplicationWindow::builder()
        .application(application)
        .title("TypeWhisper")
        .default_width(560)
        .default_height(700)
        .hide_on_close(true)
        .content(&view)
        .build();
    Ui {
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
    }
}

/// What the status card shows.
struct Hero<'a> {
    /// `None` shows a spinner instead.
    icon: Option<&'a str>,
    /// Style class of the icon: `accent`, `error`, `warning` or `dim-label`.
    tone: &'a str,
    title: &'a str,
    text: &'a str,
    /// A key shown after `text`, followed by `text_after`.
    key: &'a str,
    text_after: &'a str,
}

impl<'a> Hero<'a> {
    fn new(icon: Option<&'a str>, tone: &'a str, title: &'a str, text: &'a str) -> Self {
        Self {
            icon,
            tone,
            title,
            text,
            key: "",
            text_after: "",
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

    /// Sends a state to the indicator; `idle` hides it.
    fn indicator(&self, state: &str, text: &str) {
        let mut app_state = self.state.borrow_mut();
        app_state.indicator_generation += 1;
        if let Some(bridge) = app_state.bridge.as_ref() {
            bridge.set_state(state, text);
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

    /// The app the last text went to, when the GNOME Shell extension reports it.
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
        self.ui.typing_row.set_subtitle(if approved {
            "Pastes text into other apps."
        } else {
            "Pastes text into other apps. In the desktop's dialog, switch on remote interaction and clipboard access."
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
                (Some(error), _) => format!("Not set: {error}"),
                (None, Some("")) => {
                    "No key assigned. Assign one in the desktop's keyboard settings.".to_owned()
                }
                _ => "Short press to toggle, hold to push-to-talk.".to_owned(),
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
            ModelState::Searching => "Looking for the Parakeet plugin…".to_owned(),
            ModelState::Unavailable(error) | ModelState::Failed(error) => error.clone(),
            ModelState::Lost => "The speech model stopped unexpectedly.".to_owned(),
            ModelState::NotDownloaded => {
                format!("{MODEL_SUMMARY} Connect to the internet to download it (465 MB).")
            }
            ModelState::Downloading(Some(_)) => "Downloading…".to_owned(),
            ModelState::Downloading(None) => "Unpacking…".to_owned(),
            ModelState::Loading => "Loading model…".to_owned(),
            ModelState::Ready => MODEL_SUMMARY.to_owned(),
        };
        ui.model_row.set_subtitle(&subtitle);
        ui.model_button.set_visible(matches!(
            model,
            ModelState::NotDownloaded | ModelState::Failed(_) | ModelState::Lost
        ));
        ui.model_button.set_label(if model == ModelState::Lost {
            "Restart"
        } else {
            "Download"
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
                "No connection to the desktop",
                error,
            )
        } else if state.phase == Phase::Recording {
            Hero::new(Some("media-record-symbolic"), "error", "Recording", "")
        } else if state.phase == Phase::Transcribing {
            Hero::new(None, "", "Transcribing…", "")
        } else if state.starting {
            Hero::new(None, "", "Starting…", "")
        } else if !approved {
            Hero::new(
                Some("edit-paste-symbolic"),
                "dim-label",
                "Needs Access",
                "Access is still needed before text can be pasted into other apps.",
            )
        } else if key.is_none_or(str::is_empty) {
            Hero::new(
                Some("input-keyboard-symbolic"),
                "dim-label",
                "No hotkey",
                "A hotkey still needs to be set before dictation can start.",
            )
        } else {
            match &state.model {
                ModelState::Searching | ModelState::Loading => {
                    Hero::new(None, "", "Loading model…", "")
                }
                ModelState::Downloading(_) => Hero::new(None, "", "Downloading model…", ""),
                ModelState::NotDownloaded => Hero::new(
                    Some("folder-download-symbolic"),
                    "dim-label",
                    "No model loaded",
                    "A speech model still needs to be downloaded before dictation can run.",
                ),
                ModelState::Unavailable(error) | ModelState::Failed(error) => Hero::new(
                    Some("dialog-warning-symbolic"),
                    "warning",
                    "No model loaded",
                    error,
                ),
                ModelState::Lost => Hero::new(
                    Some("dialog-warning-symbolic"),
                    "warning",
                    "No model loaded",
                    "The speech model stopped unexpectedly. Restart it below.",
                ),
                ModelState::Ready => Hero {
                    key: key.unwrap_or_default(),
                    text_after: "in any app to begin.",
                    ..Hero::new(
                        Some("audio-input-microphone-symbolic"),
                        "accent",
                        "Ready to start dictating?",
                        "Press",
                    )
                },
            }
        };

        let ui = &self.ui;
        ui.hero_icon.set_visible(hero.icon.is_some());
        ui.hero_spinner.set_visible(hero.icon.is_none());
        if let Some(icon) = hero.icon {
            ui.hero_icon.set_icon_name(Some(icon));
            ui.hero_icon.set_css_classes(&[hero.tone]);
        }
        ui.hero_title.set_label(hero.title);
        for (label, text) in [
            (&ui.hero_text, hero.text),
            (&ui.hero_key, hero.key),
            (&ui.hero_text_after, hero.text_after),
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

async fn bind_shortcut(app: Rc<App>) {
    let Some(connection) = app.state.borrow().connection.clone() else {
        return;
    };
    app.ui.shortcut_button.set_sensitive(false);
    match shortcuts::bind(&connection, SHORTCUT_DESCRIPTION).await {
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

fn missing_plugin() -> &'static str {
    if std::env::var_os("FLATPAK_ID").is_some() {
        "The Parakeet add-on is not installed. Install com.typewhisper.TypeWhisper.Plugin.Parakeet."
    } else {
        "The Parakeet plugin is not installed."
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
    .unwrap_or_else(|_| Err("The plugin host crashed.".into()));

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
        Some("Still working on the last dictation.")
    } else {
        match state.model {
            ModelState::Ready => None,
            ModelState::Searching | ModelState::Loading => Some("Loading model…"),
            _ => Some("No model loaded"),
        }
    };
    if let Some(text) = refusal {
        let busy = state.busy;
        state.interpreter.reset();
        drop(state);
        if busy {
            // The indicator already shows the running dictation. Error feedback
            // would replace it and hide it before the text is inserted.
            app.toast(text);
        } else {
            app.dictation_failed(text);
        }
        return;
    }
    match Recorder::start() {
        Ok(recorder) => {
            state.recorder = Some(recorder);
            drop(state);
            app.set_phase(Phase::Recording);
            app.indicator("recording", "Recording");
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
    app.indicator("processing", "Transcribing…");
    glib::spawn_future_local(async move {
        let transcribed = gio::spawn_blocking(move || -> Result<String, CallError> {
            let samples = recorder.stop().map_err(CallError::Failed)?;
            if samples.len() < MIN_SAMPLES {
                return Ok(String::new());
            }
            let plugin =
                plugin.ok_or(CallError::Failed("The speech model is not loaded.".into()))?;
            let mut process = plugin.lock().map_err(|_| CallError::Lost)?;
            let result = process.transcribe(&samples, None)?;
            Ok(result["text"].as_str().unwrap_or_default().to_owned())
        })
        .await
        .unwrap_or(Err(CallError::Lost));

        // What the indicator ends on: `Ok` for inserted text, `Err` with the reason otherwise.
        let outcome = match transcribed {
            Ok(text) if text.is_empty() => Err("No speech detected".to_owned()),
            Ok(text) => {
                app.ui.last_row.set_title(&text);
                app.ui.last_row.set_subtitle("");
                app.ui.last_group.set_visible(true);
                match inserter {
                    Some(inserter) => {
                        let inserted = inserter.insert(&text).await;
                        app.refresh_typing_row();
                        match inserted {
                            Ok(()) => {
                                app.ui.last_row.set_subtitle(&app.focused_target());
                                Ok(())
                            }
                            Err(error) => Err(error.to_string()),
                        }
                    }
                    None => Err("No connection to the desktop portals.".to_owned()),
                }
            }
            Err(CallError::Failed(error)) => Err(error),
            Err(CallError::Lost) => {
                // The process is gone or in an unknown state. Start a new one,
                // which loads the model again.
                app.forget_plugin();
                app.set_model(ModelState::Searching);
                glib::spawn_future_local(start_plugin(Rc::clone(&app)));
                Err("The speech model stopped. Starting it again…".to_owned())
            }
        };
        app.state.borrow_mut().busy = false;
        app.set_phase(Phase::Idle);
        match outcome {
            Ok(()) => app.indicator_feedback("inserting", "Inserted", INSERTED_FEEDBACK),
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
