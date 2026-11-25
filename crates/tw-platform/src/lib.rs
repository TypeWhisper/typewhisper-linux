//! Linux desktop services for TypeWhisper.
//!
//! Everything that needs the desktop's permission goes through the XDG desktop
//! portals, so the same code works on GNOME and KDE Plasma and inside Flatpak.

pub mod audio;
pub mod insert;
pub mod portal;
pub mod shortcuts;

/// The D-Bus library the portal types use, for callers that hold a connection.
pub use ashpd::zbus;

use std::path::PathBuf;

/// `$XDG_STATE_HOME/typewhisper`, created on first use.
pub fn state_dir() -> std::io::Result<PathBuf> {
    xdg_dir("XDG_STATE_HOME", ".local/state")
}

/// `$XDG_DATA_HOME/typewhisper`, created on first use.
pub fn data_dir() -> std::io::Result<PathBuf> {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

/// The XDG base directory spec treats empty and relative values as unset.
fn xdg_dir(variable: &str, fallback: &str) -> std::io::Result<PathBuf> {
    let base = std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(fallback)))
        .ok_or_else(|| std::io::Error::other(format!("neither {variable} nor HOME is set")))?;
    let dir = base.join("typewhisper");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
