//! The app: a setup window, the dictation shortcut, and the flow from
//! recording to transcription to insertion.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

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

struct Ui {
    status: adw::ActionRow,
    last_text: gtk::Label,
    shortcut_row: adw::ActionRow,
    shortcut_button: gtk::Button,
    typing_row: adw::ActionRow,
    typing_button: gtk::Button,
    model_row: adw::ActionRow,
    model_button: gtk::Button,
}

#[derive(Default)]
struct State {
    connection: Option<zbus::Connection>,
    inserter: Option<Rc<TextInserter>>,
    shortcut: Option<BoundShortcut>,
    interpreter: PressInterpreter,
    recorder: Option<Recorder>,
    plugin: Option<Arc<Mutex<PluginProcess>>>,
    model_ready: bool,
    busy: bool,
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

    let ui = build_ui(application);
    let bridge = application
        .dbus_connection()
        .and_then(|c| ShellBridge::register(c).ok());
    let app = Rc::new(App {
        ui,
        state: RefCell::new(State {
            bridge,
            ..Default::default()
        }),
    });

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
    // Subtitles show text from the desktop and from errors, for example
    // "Press <Control><Alt>d", which is no valid markup.
    let row = |title: &str, subtitle: &str| {
        adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .use_markup(false)
            .build()
    };
    let button = |label: &str| {
        gtk::Button::builder()
            .label(label)
            .valign(gtk::Align::Center)
            .build()
    };

    let status = row("Status", "Starting…");
    let last_text = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .selectable(true)
        .css_classes(["dim-label"])
        .label("Nothing dictated yet.")
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let dictation = adw::PreferencesGroup::builder().title("Dictation").build();
    dictation.add(&status);
    let last = adw::PreferencesGroup::builder()
        .title("Last dictation")
        .build();
    last.add(&last_text);

    let shortcut_row = row("Dictation shortcut", "Not set up");
    let shortcut_button = button("Set Up");
    shortcut_row.add_suffix(&shortcut_button);
    let typing_row = row("Typing into other apps", "Not allowed yet");
    let typing_button = button("Allow");
    typing_row.add_suffix(&typing_button);
    let model_row = row("Speech model", "Looking for the Parakeet plugin…");
    let model_button = button("Download");
    model_button.set_sensitive(false);
    model_row.add_suffix(&model_button);
    let setup = adw::PreferencesGroup::builder()
        .title("Setup")
        .description(
            "Hold the shortcut while you speak, or tap it to start and tap again to stop. \
             The text goes into the app you are typing in.",
        )
        .build();
    setup.add(&shortcut_row);
    setup.add(&typing_row);
    setup.add(&model_row);

    let page = adw::PreferencesPage::new();
    page.add(&dictation);
    page.add(&setup);
    page.add(&last);

    let menu = gio::Menu::new();
    menu.append(Some("Quit"), Some("app.quit"));
    let header = adw::HeaderBar::new();
    header.pack_end(
        &gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .build(),
    );
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&page));

    // The application keeps the window; activate() presents it again.
    let _window = adw::ApplicationWindow::builder()
        .application(application)
        .title("TypeWhisper")
        .default_width(560)
        .default_height(640)
        .hide_on_close(true)
        .content(&view)
        .build();
    Ui {
        status,
        last_text,
        shortcut_row,
        shortcut_button,
        typing_row,
        typing_button,
        model_row,
        model_button,
    }
}

impl App {
    fn set_status(&self, text: &str) {
        self.ui.status.set_subtitle(text);
    }

    fn indicator(&self, state: &str, text: &str) {
        if let Some(bridge) = self.state.borrow().bridge.as_ref() {
            bridge.set_state(state, text);
        }
    }

    /// Names the app the text went to when the GNOME Shell extension reports it.
    fn inserted_status(&self) -> String {
        let focused = self
            .state
            .borrow()
            .bridge
            .as_ref()
            .map(|b| b.focused_app())
            .unwrap_or_default();
        let target = if focused.title.is_empty() {
            focused.app_id
        } else {
            focused.title
        };
        if target.is_empty() {
            "Ready".to_owned()
        } else {
            format!("Ready. The last dictation went to {target}.")
        }
    }

    /// The model is loaded and no dictation is running.
    fn is_idle(&self) -> bool {
        let state = self.state.borrow();
        state.model_ready && state.recorder.is_none() && !state.busy
    }

    /// Drops a plugin process that died or no longer answers in order.
    fn forget_plugin(&self) {
        let mut state = self.state.borrow_mut();
        state.plugin = None;
        state.model_ready = false;
    }

    /// After the plugin died while setting up the model. A click on the button
    /// starts it again; there is no automatic restart, so a model that crashes
    /// the plugin on every load cannot cause a loop.
    fn offer_restart(&self) {
        self.forget_plugin();
        let text = "The speech model stopped unexpectedly.";
        self.ui.model_row.set_subtitle(text);
        self.set_status(text);
        self.ui.model_button.set_label("Restart");
        self.ui.model_button.set_visible(true);
        self.ui.model_button.set_sensitive(true);
    }

    fn refresh_typing_row(&self) {
        let approved = self
            .state
            .borrow()
            .inserter
            .as_ref()
            .is_some_and(|i| i.is_approved());
        if approved {
            self.ui.typing_row.set_subtitle("Allowed");
            self.ui.typing_button.set_visible(false);
        } else {
            self.ui.typing_row.set_subtitle(
                "Not allowed yet. In the desktop's dialog, switch on remote interaction and clipboard access.",
            );
            self.ui.typing_button.set_visible(true);
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
            app.set_status(&format!("No connection to the desktop portals: {error}"));
            return;
        }
    }
    app.refresh_typing_row();
    if shortcut_flag().is_some_and(|f| f.exists()) {
        bind_shortcut(Rc::clone(&app)).await;
    }
    start_plugin(Rc::clone(&app)).await;
}

async fn bind_shortcut(app: Rc<App>) {
    let Some(connection) = app.state.borrow().connection.clone() else {
        return;
    };
    app.ui.shortcut_button.set_sensitive(false);
    match shortcuts::bind(&connection, SHORTCUT_DESCRIPTION).await {
        Ok((bound, mut events)) => {
            let trigger = if bound.trigger_description.is_empty() {
                "Set up, but no key assigned. Assign one in the desktop's keyboard settings."
                    .to_owned()
            } else {
                shortcuts::readable_trigger(&bound.trigger_description)
            };
            app.ui.shortcut_row.set_subtitle(&trigger);
            app.ui.shortcut_button.set_visible(false);
            if let Some(flag) = shortcut_flag() {
                let _ = std::fs::write(flag, "");
            }
            app.state.borrow_mut().shortcut = Some(bound);
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
        Err(error) => {
            app.ui
                .shortcut_row
                .set_subtitle(&format!("Not set up: {error}"));
            app.ui.shortcut_button.set_sensitive(true);
        }
    }
}

async fn approve_typing(app: Rc<App>) {
    let Some(inserter) = app.state.borrow().inserter.clone() else {
        return;
    };
    app.ui.typing_button.set_sensitive(false);
    match inserter.approve().await {
        // Keep "Listening…" or "Transcribing…" while a dictation runs.
        Ok(()) if app.is_idle() => app.set_status("Ready"),
        Ok(()) => {}
        Err(error) => app.set_status(&error.to_string()),
    }
    app.ui.typing_button.set_sensitive(true);
    app.refresh_typing_row();
}

type PluginStart = Result<(PluginProcess, bool), String>;

async fn start_plugin(app: Rc<App>) {
    let started: PluginStart = gio::spawn_blocking(|| {
        let data = tw_platform::data_dir().map_err(|e| e.to_string())?;
        let plugins = discover(&plugin_dirs(&data.join("plugins")));
        let parakeet = plugins
            .iter()
            .find(|p| p.manifest.id == PARAKEET)
            .ok_or("The Parakeet plugin is not installed.")?;
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
                app.ui
                    .model_row
                    .set_subtitle("NVIDIA Parakeet TDT 0.6B v3, not downloaded (465 MB)");
                app.ui.model_button.set_label("Download");
                app.ui.model_button.set_sensitive(true);
                app.set_status("Download the speech model to start.");
            }
        }
        Err(error) => {
            app.ui.model_row.set_subtitle(&error);
            app.set_status(&error);
        }
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
    app.ui
        .model_row
        .set_subtitle("NVIDIA Parakeet TDT 0.6B v3, loading…");
    let loaded = gio::spawn_blocking(move || {
        call_plugin(&plugin, "transcription.loadModel", json!({"threads": 4}))
    })
    .await
    .unwrap_or(Err(CallError::Lost));
    match loaded {
        Ok(_) => {
            app.state.borrow_mut().model_ready = true;
            app.ui
                .model_row
                .set_subtitle("NVIDIA Parakeet TDT 0.6B v3, ready");
            app.ui.model_button.set_visible(false);
            app.set_status("Ready");
        }
        Err(CallError::Failed(error)) => {
            app.ui.model_row.set_subtitle(&error);
            app.ui.model_button.set_sensitive(true);
        }
        Err(CallError::Lost) => app.offer_restart(),
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
    app.ui.model_button.set_sensitive(false);
    app.ui.model_row.set_subtitle("Downloading…");
    let (progress_tx, progress_rx) = async_channel::unbounded::<String>();
    glib::spawn_future_local(glib::clone!(
        #[strong]
        app,
        async move {
            while let Ok(text) = progress_rx.recv().await {
                app.ui.model_row.set_subtitle(&text);
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
                let text = if params["unpacking"].as_bool() == Some(true) {
                    "Unpacking…".to_owned()
                } else {
                    let received = params["receivedBytes"].as_f64().unwrap_or(0.0);
                    let total = params["totalBytes"].as_f64().unwrap_or(1.0).max(1.0);
                    format!("Downloading {:.0} %", 100.0 * received / total)
                };
                let _ = progress_tx.send_blocking(text);
            },
        )?)
    })
    .await
    .unwrap_or(Err(CallError::Lost));
    match result {
        Ok(_) => load_model(app).await,
        Err(CallError::Failed(error)) => {
            app.ui.model_row.set_subtitle(&error);
            app.ui.model_button.set_sensitive(true);
        }
        Err(CallError::Lost) => app.offer_restart(),
    }
}

fn start_recording(app: &Rc<App>) {
    let mut state = app.state.borrow_mut();
    let refusal = if state.busy {
        Some("Still working on the last dictation.")
    } else if !state.model_ready {
        Some("Download the speech model first.")
    } else {
        None
    };
    if let Some(text) = refusal {
        state.interpreter.reset();
        drop(state);
        app.set_status(text);
        return;
    }
    match Recorder::start() {
        Ok(recorder) => {
            state.recorder = Some(recorder);
            drop(state);
            app.set_status("Listening…");
            app.indicator("recording", "Listening…");
        }
        Err(error) => {
            state.interpreter.reset();
            drop(state);
            app.set_status(&error);
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
    app.set_status("Transcribing…");
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

        match transcribed {
            Ok(text) if text.is_empty() => app.set_status("Nothing was heard."),
            Ok(text) => {
                app.ui.last_text.set_label(&text);
                app.ui.last_text.remove_css_class("dim-label");
                match inserter {
                    Some(inserter) => {
                        app.indicator("processing", "Inserting…");
                        match inserter.insert(&text).await {
                            Ok(()) => app.set_status(&app.inserted_status()),
                            Err(error) => app.set_status(&error.to_string()),
                        }
                        app.refresh_typing_row();
                    }
                    None => app.set_status("No connection to the desktop portals."),
                }
            }
            Err(CallError::Failed(error)) => app.set_status(&error),
            Err(CallError::Lost) => {
                // The process is gone or in an unknown state. Start a new one,
                // which loads the model again.
                app.forget_plugin();
                app.set_status("The speech model stopped. Starting it again…");
                glib::spawn_future_local(start_plugin(Rc::clone(&app)));
            }
        }
        app.state.borrow_mut().busy = false;
        app.indicator("idle", "");
    });
}

enum CallError {
    /// The plugin or the recording reported an error; the process is fine.
    Failed(String),
    /// The plugin process died, panicked a worker or no longer answers in order.
    Lost,
}

impl From<PluginError> for CallError {
    fn from(error: PluginError) -> Self {
        match error {
            PluginError::Io(_) | PluginError::Protocol(_) => CallError::Lost,
            other => CallError::Failed(other.to_string()),
        }
    }
}
