//! D-Bus side of the TypeWhisper GNOME Shell extension.
//!
//! The extension draws the dictation indicator from `StateChanged` and reports
//! the focused app with `FocusChanged`. It only talks to the owner of the app's
//! bus name, which GApplication claims at startup.

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
    <signal name="StateChanged">
      <arg type="u" name="version"/>
      <arg type="s" name="state"/>
      <arg type="s" name="text"/>
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
}

impl ShellBridge {
    pub fn register(connection: gio::DBusConnection) -> Result<Self, glib::Error> {
        let node = gio::DBusNodeInfo::for_xml(XML)?;
        let interface = node.lookup_interface(INTERFACE).expect("interface in XML");
        let focused = Rc::new(RefCell::new(FocusedApp::default()));
        let target = Rc::clone(&focused);
        let registration = connection
            .register_object(PATH, &interface)
            .method_call(move |_, _sender, _, _, method, params, invocation| {
                if method == "FocusChanged"
                    && let Some((version, app_id, title)) = params.get::<(u32, String, String)>()
                    && version == PROTOCOL_VERSION
                {
                    *target.borrow_mut() = FocusedApp { app_id, title };
                }
                invocation.return_value(None);
            })
            .build()?;
        Ok(Self {
            connection,
            _registration: registration,
            focused,
        })
    }

    /// `state` is `idle`, `recording` or `processing`.
    pub fn set_state(&self, state: &str, text: &str) {
        let _ = self.connection.emit_signal(
            None,
            PATH,
            INTERFACE,
            "StateChanged",
            Some(&(PROTOCOL_VERSION, state, text).to_variant()),
        );
    }

    pub fn focused_app(&self) -> FocusedApp {
        self.focused.borrow().clone()
    }
}
