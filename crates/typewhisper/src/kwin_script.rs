//! Focus reports on KDE Plasma. KWin names the active window only to its own
//! scripts, so TypeWhisper loads one that calls `WindowActivated` on the app
//! whenever another window becomes active, and unloads it when the app quits.
//! A script left over from a run that crashed is replaced at the next start.
//!
//! Inside Flatpak the app may not talk to KWin, so there are no reports there.

use std::path::PathBuf;

use gtk::gio;
use gtk::glib::{self, prelude::ToVariant};

use crate::shell_bridge;

const KWIN: &str = "org.kde.KWin";
const SCRIPTING_PATH: &str = "/Scripting";
const SCRIPTING: &str = "org.kde.kwin.Scripting";
/// The name KWin knows the script by.
const PLUGIN: &str = "typewhisper-focus";
const TIMEOUT_MS: i32 = 2000;

fn source() -> String {
    format!(
        r#"// Reports the active window to TypeWhisper, which loaded this script.
function report(window) {{
    callDBus("{service}", "{path}", "{interface}", "WindowActivated",
        window ? String(window.desktopFileName || window.resourceClass || "") : "",
        window ? String(window.caption || "") : "");
}}
workspace.windowActivated.connect(report);
report(workspace.activeWindow);
"#,
        service = crate::APP_ID,
        path = shell_bridge::PATH,
        interface = shell_bridge::INTERFACE,
    )
}

/// KWin reads the file when the script starts, so it stays until the unload.
fn script_path() -> PathBuf {
    glib::user_runtime_dir()
        .join("typewhisper")
        .join("kwin-focus.js")
}

/// KWin's unique bus name, the sender of the script's reports. `None` when KWin
/// does not run or the app runs inside Flatpak.
pub async fn kwin_name(connection: &gio::DBusConnection) -> Option<String> {
    if std::env::var_os("FLATPAK_ID").is_some() {
        return None;
    }
    let reply = connection
        .call_future(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetNameOwner",
            Some(&(KWIN,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            TIMEOUT_MS,
        )
        .await
        .ok()?;
    reply.get::<(String,)>().map(|(name,)| name)
}

/// Loads and starts the script.
pub async fn load(connection: &gio::DBusConnection) -> Result<(), glib::Error> {
    let path = script_path();
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, source()));
    if let Err(error) = written {
        return Err(glib::Error::new(
            gio::IOErrorEnum::Failed,
            &format!("{}: {error}", path.display()),
        ));
    }
    let _ = scripting(connection, "unloadScript", (PLUGIN,).to_variant()).await;
    let reply = scripting(
        connection,
        "loadScript",
        (path.to_string_lossy().as_ref(), PLUGIN).to_variant(),
    )
    .await?;
    let id = reply.get::<(i32,)>().map_or(-1, |(id,)| id);
    if id < 0 {
        return Err(glib::Error::new(
            gio::IOErrorEnum::Failed,
            "KWin did not load the script",
        ));
    }
    connection
        .call_future(
            Some(KWIN),
            &format!("{SCRIPTING_PATH}/Script{id}"),
            "org.kde.kwin.Script",
            "run",
            None,
            None,
            gio::DBusCallFlags::NONE,
            TIMEOUT_MS,
        )
        .await?;
    Ok(())
}

/// Unloads the script. Runs while the app quits, so it waits briefly at most.
pub fn unload(connection: &gio::DBusConnection) {
    let _ = connection.call_sync(
        Some(KWIN),
        SCRIPTING_PATH,
        SCRIPTING,
        "unloadScript",
        Some(&(PLUGIN,).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        500,
        None::<&gio::Cancellable>,
    );
    let _ = std::fs::remove_file(script_path());
}

async fn scripting(
    connection: &gio::DBusConnection,
    method: &str,
    parameters: glib::Variant,
) -> Result<glib::Variant, glib::Error> {
    connection
        .call_future(
            Some(KWIN),
            SCRIPTING_PATH,
            SCRIPTING,
            method,
            Some(&parameters),
            None,
            gio::DBusCallFlags::NONE,
            TIMEOUT_MS,
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_calls_the_bridge() {
        let source = source();
        assert!(source.contains(r#"callDBus("com.typewhisper.TypeWhisper", "/com/typewhisper/TypeWhisper", "com.typewhisper.TypeWhisper.Shell", "WindowActivated","#));
    }
}
