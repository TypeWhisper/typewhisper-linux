//! Inserting text into the focused app through the RemoteDesktop and Clipboard portals.
//!
//! The text goes on the clipboard through the Clipboard portal, then the portal's
//! virtual keyboard presses Ctrl+V. Typing characters one by one is no option:
//! keysyms that the active layout lacks (umlauts on a US layout, emoji, CJK) are
//! dropped by the compositor.
//!
//! A session is opened per insertion and closed right after, because GNOME shows
//! a remote-control indicator while one is open. The restore token from the first
//! approval lets later sessions start without a dialog.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use ashpd::desktop::clipboard::{Clipboard, RequestClipboardOptions, SetSelectionOptions};
use ashpd::desktop::remote_desktop::{
    DeviceType, KeyState, NotifyKeyboardKeysymOptions, RemoteDesktop, SelectDevicesOptions,
    StartOptions,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode, Session};
use ashpd::zbus;
use async_io::Timer;
use futures_util::{StreamExt, future, pin_mut};

const KEYSYM_CONTROL_L: i32 = 0xffe3;
const KEYSYM_V: i32 = 0x76;
const MIME_TYPES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "TEXT",
    "STRING",
];
/// Gap between key events. Without it, GNOME pasted nothing in tests.
const KEY_GAP: Duration = Duration::from_millis(50);
/// Time the target app gets to fetch the clipboard after Ctrl+V.
const PASTE_GRACE: Duration = Duration::from_millis(400);

#[derive(Debug)]
pub enum InsertError {
    /// The user denied the request or left keyboard or clipboard access switched off.
    NotAllowed(&'static str),
    Portal(ashpd::Error),
}

impl std::fmt::Display for InsertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InsertError::NotAllowed(what) => write!(f, "{what}"),
            InsertError::Portal(error) => write!(f, "The desktop refused the insertion: {error}"),
        }
    }
}

impl From<ashpd::Error> for InsertError {
    fn from(error: ashpd::Error) -> Self {
        InsertError::Portal(error)
    }
}

pub struct TextInserter {
    connection: zbus::Connection,
    token_file: PathBuf,
}

impl TextInserter {
    pub fn new(connection: zbus::Connection, token_file: PathBuf) -> Self {
        Self {
            connection,
            token_file,
        }
    }

    /// True once the user has approved remote control and the desktop gave a restore token.
    pub fn is_approved(&self) -> bool {
        self.token_file.exists()
    }

    /// Asks for approval (dialog on first use) without inserting anything.
    pub async fn approve(&self) -> Result<(), InsertError> {
        let (_, _, session) = self.open().await?;
        let _ = session.close().await;
        Ok(())
    }

    /// Pastes `text` into the focused app.
    pub async fn insert(&self, text: &str) -> Result<(), InsertError> {
        let (remote, clipboard, session) = self.open().await?;
        let result = paste(&remote, &clipboard, &session, text.as_bytes()).await;
        let _ = session.close().await;
        result
    }

    async fn open(
        &self,
    ) -> Result<(RemoteDesktop, Clipboard, Session<RemoteDesktop>), InsertError> {
        let remote = RemoteDesktop::with_connection(self.connection.clone()).await?;
        let clipboard = Clipboard::with_connection(self.connection.clone()).await?;
        let session = remote
            .create_session(CreateSessionOptions::default())
            .await?;
        if let Err(error) = self.start(&remote, &clipboard, &session).await {
            let _ = session.close().await;
            return Err(error);
        }
        Ok((remote, clipboard, session))
    }

    async fn start(
        &self,
        remote: &RemoteDesktop,
        clipboard: &Clipboard,
        session: &Session<RemoteDesktop>,
    ) -> Result<(), InsertError> {
        clipboard
            .request(session, RequestClipboardOptions::default())
            .await?;
        let token = std::fs::read_to_string(&self.token_file).ok();
        let mut options = SelectDevicesOptions::default()
            .set_devices(DeviceType::KEYBOARD)
            .set_persist_mode(PersistMode::ExplicitlyRevoked);
        if let Some(token) = token.as_deref() {
            options = options.set_restore_token(token.trim());
        }
        remote.select_devices(session, options).await?.response()?;
        let selected = remote
            .start(session, None, StartOptions::default())
            .await?
            .response()?;
        let refusal = if !selected.devices().contains(DeviceType::KEYBOARD) {
            Some("TypeWhisper may not type. Allow remote interaction in the desktop's dialog.")
        } else if !selected.is_clipboard_enabled() {
            Some(
                "TypeWhisper may not use the clipboard. Allow clipboard access in the desktop's dialog.",
            )
        } else {
            None
        };
        // Every start returns a new token; the old one is used up. A token for an
        // incomplete grant is dropped, so the next attempt shows the dialog again
        // and the app does not count it as approved.
        match (refusal, selected.restore_token()) {
            (None, Some(token)) => write_private(&self.token_file, token),
            _ => {
                let _ = std::fs::remove_file(&self.token_file);
            }
        }
        match refusal {
            Some(text) => Err(InsertError::NotAllowed(text)),
            None => Ok(()),
        }
    }
}

async fn paste(
    remote: &RemoteDesktop,
    clipboard: &Clipboard,
    session: &Session<RemoteDesktop>,
    bytes: &[u8],
) -> Result<(), InsertError> {
    let mut transfers = std::pin::pin!(
        clipboard
            .receive_selection_transfer::<RemoteDesktop>()
            .await?
    );
    clipboard
        .set_selection(
            session,
            SetSelectionOptions::default().set_mime_types(MIME_TYPES),
        )
        .await?;

    // The compositor asks for the data right away and again when the target
    // pastes, so transfers are served while the keys are pressed.
    let serve = async {
        // Portals send session signals only to the session's owner, and this
        // connection has no other remote desktop session open.
        while let Some((_, _mime, serial)) = transfers.next().await {
            let written = match clipboard.selection_write(session, serial).await {
                Ok(fd) => std::fs::File::from(std::os::fd::OwnedFd::from(fd))
                    .write_all(bytes)
                    .is_ok(),
                Err(_) => false,
            };
            let _ = clipboard
                .selection_write_done(session, serial, written)
                .await;
        }
    };
    let press = async {
        Timer::after(KEY_GAP * 2).await;
        for (keysym, state) in [
            (KEYSYM_CONTROL_L, KeyState::Pressed),
            (KEYSYM_V, KeyState::Pressed),
            (KEYSYM_V, KeyState::Released),
            (KEYSYM_CONTROL_L, KeyState::Released),
        ] {
            remote
                .notify_keyboard_keysym(
                    session,
                    keysym,
                    state,
                    NotifyKeyboardKeysymOptions::default(),
                )
                .await?;
            Timer::after(KEY_GAP).await;
        }
        Timer::after(PASTE_GRACE).await;
        Ok::<(), InsertError>(())
    };
    pin_mut!(serve, press);
    match future::select(press, serve).await {
        future::Either::Left((result, _)) => result,
        future::Either::Right(((), press)) => press.await,
    }
}

fn write_private(path: &std::path::Path, contents: &str) {
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(contents.as_bytes()));
}
