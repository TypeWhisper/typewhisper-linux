//! The dictation shortcut through the GlobalShortcuts portal.
//!
//! The desktop owns the key binding: the app suggests a trigger, the user
//! confirms or changes it in a system dialog, and later sessions bind silently.

use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use ashpd::desktop::{CreateSessionOptions, Session};
use ashpd::zbus;
use futures_util::{Stream, StreamExt, future, stream};

pub const DICTATE: &str = "dictate";
/// XDG shortcuts format. KDE Plasma ignores GTK-style triggers such as
/// `<Control><Alt>d` and registers the shortcut without a key.
pub const DEFAULT_TRIGGER: &str = "CTRL+ALT+d";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutEvent {
    Pressed,
    Released,
}

/// Keeps the portal session alive; the shortcut stops working when it is dropped.
pub struct BoundShortcut {
    _portal: GlobalShortcuts,
    _session: Session<GlobalShortcuts>,
    /// What the desktop shows for the binding: `Press <Control><Alt>d` on GNOME,
    /// `Ctrl+Alt+D` on KDE. Empty when the user removed the key. See
    /// [`readable_trigger`].
    pub trigger_description: String,
}

/// Binds the dictation shortcut. Shows the desktop's dialog the first time.
pub async fn bind(
    connection: &zbus::Connection,
    description: &str,
) -> ashpd::Result<(BoundShortcut, impl Stream<Item = ShortcutEvent> + use<>)> {
    let portal = GlobalShortcuts::with_connection(connection.clone()).await?;
    let session = portal
        .create_session(CreateSessionOptions::default())
        .await?;
    let activated = portal.receive_activated().await?;
    let deactivated = portal.receive_deactivated().await?;

    let shortcut = NewShortcut::new(DICTATE, description).preferred_trigger(DEFAULT_TRIGGER);
    let response = portal
        .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
        .await?
        .response()?;
    let trigger_description = response
        .shortcuts()
        .iter()
        .find(|s| s.id() == DICTATE)
        .map(|s| s.trigger_description().to_owned())
        .unwrap_or_default();

    // The portal sends these signals only to the session's owner, and this
    // connection holds one shortcuts session, so the shortcut ID is enough.
    let downs = activated
        .filter(|a| future::ready(a.shortcut_id() == DICTATE))
        .map(|_| true);
    let ups = deactivated
        .filter(|d| future::ready(d.shortcut_id() == DICTATE))
        .map(|_| false);
    // GNOME repeats Activated while the key is held; only the first one counts.
    let events = stream::select(downs, ups)
        .scan(false, |held, down| {
            let event = match (down, *held) {
                (true, false) => Some(ShortcutEvent::Pressed),
                (true, true) => None,
                (false, _) => Some(ShortcutEvent::Released),
            };
            *held = down;
            future::ready(Some(event))
        })
        .filter_map(future::ready);

    Ok((
        BoundShortcut {
            _portal: portal,
            _session: session,
            trigger_description,
        },
        events,
    ))
}

/// Makes the desktop's trigger description readable.
///
/// GNOME describes the binding in GTK accelerator syntax, for example
/// `Press <Control><Alt>d`; this turns it into `Press Ctrl+Alt+D`. Descriptions
/// without accelerators, such as KDE's, pass through unchanged.
pub fn readable_trigger(description: &str) -> String {
    description
        .split(' ')
        .map(|word| readable_accelerator(word).unwrap_or_else(|| word.to_owned()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn readable_accelerator(word: &str) -> Option<String> {
    let mut rest = word.strip_prefix('<')?;
    let mut parts = Vec::new();
    loop {
        let (modifier, after) = rest.split_once('>')?;
        parts.push(match modifier.to_ascii_lowercase().as_str() {
            "control" | "ctrl" | "primary" => "Ctrl".to_owned(),
            "alt" | "mod1" => "Alt".to_owned(),
            "shift" => "Shift".to_owned(),
            "super" | "mod4" => "Super".to_owned(),
            "meta" => "Meta".to_owned(),
            "hyper" => "Hyper".to_owned(),
            _ => return None,
        });
        match after.strip_prefix('<') {
            Some(next) => rest = next,
            None => {
                rest = after;
                break;
            }
        }
    }
    if rest.is_empty() {
        return None;
    }
    let mut chars = rest.chars();
    let key = match (chars.next(), chars.next()) {
        (Some(c), None) => c.to_uppercase().collect(),
        _ => rest.to_owned(),
    };
    parts.push(key);
    Some(parts.join("+"))
}

/// Turns presses and releases into start and stop commands.
///
/// Holding the shortcut records until release. A short tap starts a recording
/// that the next press ends, so both push-to-talk and toggling work.
#[derive(Debug, Default)]
pub struct PressInterpreter {
    recording: bool,
    pressed_at: Option<std::time::Instant>,
    ignore_next_release: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Start,
    Stop,
}

/// A press shorter than this keeps recording after release.
pub const TAP_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(400);

impl PressInterpreter {
    pub fn on_event(&mut self, event: ShortcutEvent, now: std::time::Instant) -> Option<Command> {
        match event {
            ShortcutEvent::Pressed if !self.recording => {
                self.recording = true;
                self.pressed_at = Some(now);
                Some(Command::Start)
            }
            ShortcutEvent::Pressed => {
                // Second press of a tapped recording ends it; its release must not count.
                self.recording = false;
                self.pressed_at = None;
                self.ignore_next_release = true;
                Some(Command::Stop)
            }
            ShortcutEvent::Released if self.ignore_next_release => {
                self.ignore_next_release = false;
                None
            }
            ShortcutEvent::Released => match self.pressed_at.take() {
                Some(at) if self.recording && now.duration_since(at) >= TAP_THRESHOLD => {
                    self.recording = false;
                    Some(Command::Stop)
                }
                _ => None,
            },
        }
    }

    /// Call when a recording ended for another reason, for example an error.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn hold_records_until_release() {
        let t = Instant::now();
        let mut p = PressInterpreter::default();
        assert_eq!(p.on_event(ShortcutEvent::Pressed, t), Some(Command::Start));
        assert_eq!(
            p.on_event(ShortcutEvent::Released, t + Duration::from_secs(2)),
            Some(Command::Stop)
        );
    }

    #[test]
    fn readable_trigger_rewrites_gtk_accelerators() {
        assert_eq!(
            readable_trigger("Press <Control><Alt>d"),
            "Press Ctrl+Alt+D"
        );
        assert_eq!(readable_trigger("<Shift><Super>space"), "Shift+Super+space");
        assert_eq!(readable_trigger("Ctrl+Alt+D"), "Ctrl+Alt+D");
        assert_eq!(readable_trigger("Press <Unknown>d"), "Press <Unknown>d");
        assert_eq!(readable_trigger(""), "");
    }

    #[test]
    fn tap_toggles() {
        let t = Instant::now();
        let mut p = PressInterpreter::default();
        assert_eq!(p.on_event(ShortcutEvent::Pressed, t), Some(Command::Start));
        assert_eq!(
            p.on_event(ShortcutEvent::Released, t + Duration::from_millis(150)),
            None
        );
        assert_eq!(
            p.on_event(ShortcutEvent::Pressed, t + Duration::from_secs(3)),
            Some(Command::Stop)
        );
        assert_eq!(
            p.on_event(ShortcutEvent::Released, t + Duration::from_millis(3100)),
            None
        );
        assert_eq!(
            p.on_event(ShortcutEvent::Pressed, t + Duration::from_secs(5)),
            Some(Command::Start)
        );
    }
}
