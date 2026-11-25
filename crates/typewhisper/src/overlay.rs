//! The dictation indicator on compositors with wlr-layer-shell, such as KDE
//! Plasma, Sway and Hyprland.
//!
//! GNOME has no layer shell; there the GNOME Shell extension draws the
//! indicator from the `StateChanged` signal. This window draws the same dark
//! pill with the same states, as a layer surface above all windows that takes
//! neither the keyboard focus nor clicks.

use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{cairo, gdk, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::waveform::{self, BarModel};

/// Distance from the bottom of the area that panels leave free.
const EDGE_INSET: i32 = 20;
/// Transparent room around the pill for its shadow.
const SHADOW_ROOM: i32 = 16;
/// How long a message may grow before it is cut off, in characters.
const MESSAGE_CHARS: i32 = 60;

/// The look of the GNOME Shell extension's `stylesheet.css`, in GTK's terms.
const CSS: &str = "
window.tw-overlay { background: none; }

/* A dark pill at the bottom of the screen, the same in the light and dark style. */
.tw-indicator {
  background-color: rgba(0, 0, 0, 0.85);
  border: 1px solid rgba(255, 255, 255, 0.15);
  border-radius: 24px;
  min-width: 238px;
  min-height: 46px;
  padding: 0 20px;
  box-shadow: 0 5px 10px rgba(0, 0, 0, 0.3);
  color: #ffffff;
}

.tw-indicator.tw-state-error {
  min-width: 298px;
  border-color: rgba(255, 69, 58, 0.55);
}

@keyframes tw-pulse {
  from { opacity: 1; }
  to { opacity: 0.55; }
}

.tw-dot {
  min-width: 8px;
  min-height: 8px;
  border-radius: 4px;
  background-color: #ff453a;
  box-shadow: 0 0 6px rgba(255, 214, 10, 0.8);
  animation: tw-pulse 1s ease-in-out infinite alternate;
}

.tw-icon.tw-inserting { color: #32d74b; }
.tw-icon.tw-error { color: #ff453a; }

.tw-timer {
  font-size: 13px;
  font-weight: 500;
  font-feature-settings: \"tnum\";
  color: rgba(255, 255, 255, 0.8);
}

.tw-message {
  font-size: 13px;
  font-weight: 500;
  color: rgba(255, 255, 255, 0.92);
}

.tw-phase {
  font-size: 13px;
  color: rgba(255, 255, 255, 0.7);
}
";

/// The states the app sends; see `ShellBridge::set_state`.
const STATES: [&str; 4] = ["recording", "processing", "inserting", "error"];

/// What the pill shows in a state, as in the GNOME Shell extension. Left:
/// what is happening (dot, spinner or icon) and, while recording, the elapsed
/// time. Right: the step the app names while it works, or the waveform of the
/// input level while recording.
#[derive(Debug, PartialEq)]
struct Parts {
    /// Style class of the pill.
    class: &'static str,
    /// The pulsing dot, the elapsed time and the waveform.
    recording: bool,
    /// The spinner and, on the right, the step.
    working: bool,
    /// The result icon and its style class.
    icon: Option<(&'static str, &'static str)>,
    /// The error message, which takes the room of the step.
    message: bool,
}

impl Parts {
    fn for_state(state: &str) -> Self {
        // A state from a newer app shows its text, like a step of processing.
        let state = if STATES.contains(&state) {
            state
        } else {
            "processing"
        };
        Self {
            class: match state {
                "recording" => "tw-state-recording",
                "inserting" => "tw-state-inserting",
                "error" => "tw-state-error",
                _ => "tw-state-processing",
            },
            recording: state == "recording",
            working: state == "processing",
            icon: match state {
                "inserting" => Some(("object-select-symbolic", "tw-inserting")),
                "error" => Some(("dialog-error-symbolic", "tw-error")),
                _ => None,
            },
            message: state == "error",
        }
    }
}

/// Minutes and seconds, as in `0:07` or `12:30`.
fn elapsed_label(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

pub struct Overlay {
    window: gtk::Window,
    pill: gtk::Box,
    dot: gtk::Box,
    spinner: adw::Spinner,
    icon: gtk::Image,
    timer: gtk::Label,
    message: gtk::Label,
    spacer: gtk::Box,
    phase: gtk::Label,
    waveform: gtk::DrawingArea,
    /// The latest input level, from 0 to 1.
    level: Rc<Cell<f64>>,
    /// Updates the elapsed time while recording.
    ticker: RefCell<Option<glib::SourceId>>,
    /// Moves the waveform's bars on every frame while recording.
    bars: RefCell<Option<gtk::TickCallbackId>>,
}

impl Overlay {
    /// `None` when the compositor has no layer shell, as on GNOME and X11.
    pub fn new() -> Option<Self> {
        if !gtk4_layer_shell::is_supported() {
            return None;
        }
        let display = gdk::Display::default()?;
        let css = gtk::CssProvider::new();
        css.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let dot = gtk::Box::builder()
            .css_classes(["tw-dot"])
            .valign(gtk::Align::Center)
            .build();
        let spinner = adw::Spinner::builder()
            .width_request(16)
            .height_request(16)
            .valign(gtk::Align::Center)
            .build();
        let icon = gtk::Image::builder()
            .pixel_size(16)
            .valign(gtk::Align::Center)
            .build();
        let label = |class: &str| {
            gtk::Label::builder()
                .css_classes([class])
                .valign(gtk::Align::Center)
                .build()
        };
        let timer = label("tw-timer");
        let message = gtk::Label::builder()
            .css_classes(["tw-message"])
            .valign(gtk::Align::Center)
            .hexpand(true)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(MESSAGE_CHARS)
            .build();
        let spacer = gtk::Box::builder().hexpand(true).build();
        let phase = label("tw-phase");
        // Decoration: the pill's name already says that it records.
        let waveform = gtk::DrawingArea::builder()
            .content_width(waveform::WIDTH as i32)
            .content_height(waveform::MAX_HEIGHT as i32)
            .valign(gtk::Align::Center)
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build();

        // The role and the name tell screen readers what the pill shows, as the
        // extension's notification role does on GNOME.
        let pill = gtk::Box::builder()
            .accessible_role(gtk::AccessibleRole::Alert)
            .css_classes(["tw-indicator"])
            .spacing(12)
            .margin_top(SHADOW_ROOM)
            .margin_bottom(SHADOW_ROOM)
            .margin_start(SHADOW_ROOM)
            .margin_end(SHADOW_ROOM)
            .build();
        for child in [
            dot.upcast_ref::<gtk::Widget>(),
            spinner.upcast_ref(),
            icon.upcast_ref(),
            timer.upcast_ref(),
            message.upcast_ref(),
            spacer.upcast_ref(),
            phase.upcast_ref(),
            waveform.upcast_ref(),
        ] {
            pill.append(child);
        }

        // Not part of the application: GtkApplication would list it among its
        // windows, and activating the app presents the first of them.
        let window = gtk::Window::builder()
            .css_classes(["tw-overlay"])
            .decorated(false)
            .resizable(false)
            .child(&pill)
            .build();
        window.init_layer_shell();
        window.set_namespace(Some("typewhisper-indicator"));
        window.set_layer(Layer::Overlay);
        window.set_anchor(Edge::Bottom, true);
        window.set_margin(Edge::Bottom, EDGE_INSET - SHADOW_ROOM);
        window.set_keyboard_mode(KeyboardMode::None);
        // Clicks go to the windows below, as with the Mac app's indicator.
        window.connect_realize(|window| {
            if let Some(surface) = window.surface() {
                surface.set_input_region(Some(&cairo::Region::create()));
            }
        });

        Some(Self {
            window,
            pill,
            dot,
            spinner,
            icon,
            timer,
            message,
            spacer,
            phase,
            waveform,
            level: Rc::default(),
            ticker: RefCell::default(),
            bars: RefCell::default(),
        })
    }

    /// The input level while recording, from 0 to 1.
    pub fn set_level(&self, level: f32) {
        self.level.set(f64::from(level));
    }

    /// Shows `state` with `text`, the same arguments as the bus signal;
    /// `idle` hides the indicator.
    pub fn set_state(&self, state: &str, text: &str) {
        self.stop_timer();
        if state == "idle" {
            self.window.set_visible(false);
            return;
        }
        let parts = Parts::for_state(state);
        self.pill.set_css_classes(&["tw-indicator", parts.class]);
        self.pill
            .update_property(&[gtk::accessible::Property::Label(text)]);
        self.dot.set_visible(parts.recording);
        self.timer.set_visible(parts.recording);
        self.timer.set_label(&elapsed_label(Duration::ZERO));
        self.spinner.set_visible(parts.working);
        self.phase.set_visible(parts.working);
        self.phase.set_label(text);
        self.icon.set_visible(parts.icon.is_some());
        if let Some((name, class)) = parts.icon {
            self.icon.set_icon_name(Some(name));
            self.icon.set_css_classes(&["tw-icon", class]);
        }
        self.message.set_visible(parts.message);
        self.message.set_label(text);
        self.spacer.set_visible(!parts.message);
        self.waveform.set_visible(parts.recording);
        // The compositor picks the screen when the window is shown. A visible
        // window stays where it is, so the indicator does not jump between
        // screens in the middle of a dictation.
        self.window.set_visible(true);
        if parts.recording {
            self.start_timer();
            self.start_waveform();
        }
    }

    /// Draws the bars from a fresh model; the tick callback runs once per
    /// frame while the pill is on screen.
    fn start_waveform(&self) {
        self.level.set(0.0);
        let heights = Rc::new(Cell::new([0.0; waveform::BAR_COUNT]));
        let drawn = Rc::clone(&heights);
        self.waveform.set_draw_func(move |_, cr, _, height| {
            draw_bars(cr, f64::from(height), &drawn.get());
        });
        let model = RefCell::new(BarModel::new());
        let level = Rc::clone(&self.level);
        let since = Instant::now();
        let tick = self.waveform.add_tick_callback(move |area, clock| {
            let now = clock.frame_time() as f64 / 1_000_000.0;
            let level = level.get();
            let bounce = since.elapsed().as_secs_f64() < waveform::BOUNCE_SECONDS
                && level < waveform::BOUNCE_LEVEL;
            let animate = area.settings().is_gtk_enable_animations();
            heights.set(model.borrow_mut().advance(now, level, bounce, animate));
            area.queue_draw();
            glib::ControlFlow::Continue
        });
        self.bars.replace(Some(tick));
    }

    fn start_timer(&self) {
        let since = Instant::now();
        let timer = self.timer.clone();
        let source = glib::timeout_add_local(Duration::from_millis(250), move || {
            let text = elapsed_label(since.elapsed());
            if timer.label() != text {
                timer.set_label(&text);
            }
            glib::ControlFlow::Continue
        });
        self.ticker.replace(Some(source));
    }

    fn stop_timer(&self) {
        if let Some(source) = self.ticker.take() {
            source.remove();
        }
        if let Some(tick) = self.bars.take() {
            tick.remove();
        }
    }
}

/// Rounded white bars, centered vertically, as in the macOS app.
fn draw_bars(cr: &cairo::Context, height: f64, heights: &[f64]) {
    let radius = waveform::BAR_WIDTH / 2.0;
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
    for (index, bar) in heights.iter().enumerate() {
        let bar_height = waveform::bar_height(*bar);
        let x = index as f64 * (waveform::BAR_WIDTH + waveform::BAR_SPACING);
        let y = (height - bar_height) / 2.0;
        cr.new_sub_path();
        cr.arc(x + radius, y + radius, radius, PI, 0.0);
        cr.arc(x + radius, y + bar_height - radius, radius, 0.0, PI);
        cr.close_path();
    }
    let _ = cr.fill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_show_what_the_extension_shows() {
        assert_eq!(
            Parts::for_state("recording"),
            Parts {
                class: "tw-state-recording",
                recording: true,
                working: false,
                icon: None,
                message: false,
            }
        );
        assert_eq!(
            Parts::for_state("processing"),
            Parts {
                class: "tw-state-processing",
                recording: false,
                working: true,
                icon: None,
                message: false,
            }
        );
        assert_eq!(
            Parts::for_state("inserting"),
            Parts {
                class: "tw-state-inserting",
                recording: false,
                working: false,
                icon: Some(("object-select-symbolic", "tw-inserting")),
                message: false,
            }
        );
        assert_eq!(
            Parts::for_state("error"),
            Parts {
                class: "tw-state-error",
                recording: false,
                working: false,
                icon: Some(("dialog-error-symbolic", "tw-error")),
                message: true,
            }
        );
    }

    #[test]
    fn unknown_states_show_their_text_as_a_step() {
        assert_eq!(
            Parts::for_state("translating"),
            Parts::for_state("processing")
        );
    }

    #[test]
    fn elapsed_time_reads_as_minutes_and_seconds() {
        assert_eq!(elapsed_label(Duration::ZERO), "0:00");
        assert_eq!(elapsed_label(Duration::from_millis(7_900)), "0:07");
        assert_eq!(elapsed_label(Duration::from_secs(65)), "1:05");
        assert_eq!(elapsed_label(Duration::from_secs(750)), "12:30");
    }
}
