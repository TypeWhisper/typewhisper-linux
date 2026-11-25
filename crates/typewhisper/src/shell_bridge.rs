//! D-Bus side of the TypeWhisper GNOME Shell extension and of the KWin script.
//!
//! The extension draws the dictation indicator from `StateChanged` and `Level` and reports
//! the focused app with `FocusChanged`. It only talks to the owner of the app's
//! bus name, which GApplication claims at startup. On KDE Plasma, the script
//! from [`crate::kwin_script`] reports the active window with `WindowActivated`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::gio;
use gtk::glib::{self, prelude::ToVariant};

pub const PATH: &str = "/com/typewhisper/TypeWhisper";
pub const INTERFACE: &str = "com.typewhisper.TypeWhisper.Shell";
pub const PROTOCOL_VERSION: u32 = 1;

const XML: &str = r#"<node>
  <interface name="com.typewhisper.TypeWhisper.Shell">
    <method name="FocusChanged">
      <arg type="u" name="version" direction="in"/>
      <arg type="s" name="app_id" direction="in"/>
      <arg type="s" name="title" direction="in"/>
    </method>
    <method name="WindowActivated">
      <arg type="s" name="app_id" direction="in"/>
      <arg type="s" name="title" direction="in"/>
    </method>
    <signal name="StateChanged">
      <arg type="u" name="version"/>
      <arg type="s" name="state"/>
      <arg type="s" name="text"/>
    </signal>
    <signal name="Level">
      <arg type="u" name="version"/>
      <arg type="d" name="level"/>
    </signal>
  </interface>
</node>"#;

#[derive(Debug, Clone, Default)]
pub struct FocusedApp {
    pub app_id: String,
    pub title: String,
}

pub struct ShellBridge {
    connection: gio::DBusConnection,
    _registration: gio::RegistrationId,
    focused: Rc<RefCell<FocusedApp>>,
    /// KWin's unique bus name once the script runs; other senders are ignored.
    kwin: Rc<RefCell<Option<String>>>,
}

impl ShellBridge {
    pub fn register(connection: gio::DBusConnection) -> Result<Self, glib::Error> {
        let node = gio::DBusNodeInfo::for_xml(XML)?;
        let interface = node.lookup_interface(INTERFACE).expect("interface in XML");
        let focused = Rc::new(RefCell::new(FocusedApp::default()));
        let kwin = Rc::new(RefCell::new(None::<String>));
        let target = Rc::clone(&focused);
        let script_sender = Rc::clone(&kwin);
        let registration = connection
            .register_object(PATH, &interface)
            .method_call(move |_, sender, _, _, method, params, invocation| {
                let report = match method {
                    "FocusChanged" => params
                        .get::<(u32, String, String)>()
                        .filter(|(version, _, _)| *version == PROTOCOL_VERSION)
                        .map(|(_, app_id, title)| (app_id, title)),
                    "WindowActivated"
                        if sender.is_some() && sender == script_sender.borrow().as_deref() =>
                    {
                        params.get::<(String, String)>()
                    }
                    _ => None,
                };
                if let Some((app_id, title)) = report {
                    *target.borrow_mut() = FocusedApp { app_id, title };
                }
                invocation.return_value(None);
            })
            .build()?;
        Ok(Self {
            connection,
            _registration: registration,
            focused,
            kwin,
        })
    }

    /// Accepts `WindowActivated` from this unique bus name, KWin's.
    pub fn accept_kwin_reports(&self, sender: String) {
        *self.kwin.borrow_mut() = Some(sender);
    }

    /// `state` is `idle`, `recording`, `processing`, `inserting` or `error`.
    /// `text` names the state in words; for `error` it is the message. An
    /// extension that does not know a state shows `text` until `idle` follows.
    pub fn set_state(&self, state: &str, text: &str) {
        let _ = self.connection.emit_signal(
            None,
            PATH,
            INTERFACE,
            "StateChanged",
            Some(&(PROTOCOL_VERSION, state, text).to_variant()),
        );
    }

    /// The input level while recording, from 0 to 1, 30 times a second. An
    /// extension that does not know the signal ignores it.
    pub fn set_level(&self, level: f32) {
        let _ = self.connection.emit_signal(
            None,
            PATH,
            INTERFACE,
            "Level",
            Some(&(PROTOCOL_VERSION, f64::from(level)).to_variant()),
        );
    }

    pub fn focused_app(&self) -> FocusedApp {
        self.focused.borrow().clone()
    }
}
