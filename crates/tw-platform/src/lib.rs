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
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| std::io::Error::other("neither XDG_STATE_HOME nor HOME is set"))?;
    let dir = base.join("typewhisper");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// `$XDG_DATA_HOME/typewhisper`, created on first use.
pub fn data_dir() -> std::io::Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or_else(|| std::io::Error::other("neither XDG_DATA_HOME nor HOME is set"))?;
    let dir = base.join("typewhisper");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
