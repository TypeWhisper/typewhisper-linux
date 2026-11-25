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
//!
//! To preserve the clipboard, the session reads what the clipboard holds before
//! the paste and offers it again afterwards. The desktop fetches it once, as it
//! does with every new selection, and keeps it after the session closes.
//!
//! Where the compositor offers Wayland data-control (KDE Plasma, wlroots), the
//! clipboard goes through that instead, see [`crate::data_control`], and the
//! portal session only presses the keys.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use ashpd::desktop::clipboard::{
    Clipboard, RequestClipboardOptions, SelectionOwnerChanged, SetSelectionOptions,
};
use ashpd::desktop::remote_desktop::{
    DeviceType, KeyState, NotifyKeyboardKeysymOptions, RemoteDesktop, SelectDevicesOptions,
    StartOptions,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode, Session};
use ashpd::zbus;
use async_io::Timer;
use futures_util::{AsyncReadExt, FutureExt, Stream, StreamExt, future, pin_mut};

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
/// The portal reports the current clipboard owner right after the session starts.
const OWNER_WAIT: Duration = Duration::from_millis(300);
/// Longest wait for one clipboard type to be read.
const READ_TIMEOUT: Duration = Duration::from_millis(500);
/// Clipboard content larger than this in total is not preserved.
const SNAPSHOT_LIMIT: u64 = 32 * 1024 * 1024;
/// Longest wait for the desktop to fetch the restored content.
const RESTORE_WAIT: Duration = Duration::from_millis(500);

/// The clipboard content before a paste, one entry per MIME type.
type Snapshot = Vec<(String, Vec<u8>)>;

#[derive(Debug)]
pub enum InsertError {
    /// The clipboard could not be set through data-control.
    Clipboard(String),
    /// The user denied the request or left keyboard or clipboard access switched off.
    NotAllowed(&'static str),
    /// No clipboard request was answered, so the text cannot have arrived.
    NotDelivered,
    Portal(ashpd::Error),
}

impl std::fmt::Display for InsertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InsertError::NotAllowed(what) => write!(f, "{what}"),
            InsertError::Clipboard(error) => write!(f, "The clipboard could not be set: {error}"),
            InsertError::NotDelivered => {
                write!(
                    f,
                    "The text did not reach the clipboard, so nothing was pasted."
                )
            }
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

    /// Pastes `text` into the focused app. With `preserve_clipboard`, the
    /// clipboard holds its previous content again afterwards.
    pub async fn insert(&self, text: &str, preserve_clipboard: bool) -> Result<(), InsertError> {
        if crate::data_control::available().await {
            return self
                .insert_with_data_control(text, preserve_clipboard)
                .await;
        }
        // Subscribed before the session starts, so the first report is not missed.
        let watcher = Clipboard::with_connection(self.connection.clone()).await?;
        let owners = if preserve_clipboard {
            Some(watcher.receive_selection_owner_changed().await?)
        } else {
            None
        };
        let (remote, clipboard, session) = self.open().await?;
        let result = match owners {
            Some(owners) => {
                pin_mut!(owners);
                let saved = snapshot(&clipboard, &session, owners.as_mut()).await;
                paste(
                    &remote,
                    &clipboard,
                    &session,
                    text.as_bytes(),
                    saved.as_ref(),
                    owners,
                )
                .await
            }
            None => {
                paste(
                    &remote,
                    &clipboard,
                    &session,
                    text.as_bytes(),
                    None,
                    futures_util::stream::pending(),
                )
                .await
            }
        };
        let _ = session.close().await;
        result
    }

    /// Sets the clipboard through data-control, presses Ctrl+V through the
    /// portal and, if asked, puts the previous content back. Without a served
    /// transfer to count, a failed paste cannot be detected here.
    async fn insert_with_data_control(
        &self,
        text: &str,
        preserve_clipboard: bool,
    ) -> Result<(), InsertError> {
        use crate::data_control;
        let saved = if preserve_clipboard {
            data_control::snapshot(SNAPSHOT_LIMIT, READ_TIMEOUT * 4).await
        } else {
            None
        };
        // The portal comes first: if it refuses, the clipboard stays untouched.
        let (remote, _clipboard, session) = self.open().await?;
        // With the setting on, clipboard managers should not keep the dictation,
        // as on macOS.
        let replaced = match data_control::offer_text(text, preserve_clipboard).await {
            Ok(replaced) => replaced,
            Err(error) => {
                let _ = session.close().await;
                return Err(InsertError::Clipboard(error));
            }
        };
        let pressed = async {
            press_ctrl_v(&remote, &session).await?;
            Timer::after(PASTE_GRACE).await;
            Ok::<(), InsertError>(())
        }
        .await;
        let _ = session.close().await;
        // The previous content comes back even when the keys failed. If another
        // app copied something meanwhile, its content stays.
        let mut restored = Ok(());
        if preserve_clipboard && !replaced.load(std::sync::atomic::Ordering::Acquire) {
            match saved {
                Some(saved) if saved.is_empty() => data_control::clear().await,
                Some(saved) => {
                    restored = data_control::offer_snapshot(saved)
                        .await
                        .map(|_| ())
                        .map_err(InsertError::Clipboard);
                }
                // Not readable in full: the dictated text stays.
                None => {}
            }
        }
        pressed?;
        restored
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

/// Reads every type the clipboard offers. `None` when the content cannot be
/// read in full or is too large; then nothing is restored.
///
/// GNOME reports no owner for an empty clipboard. An empty clipboard comes back
/// as empty text, because an empty selection does not clear it there.
async fn snapshot(
    clipboard: &Clipboard,
    session: &Session<RemoteDesktop>,
    mut owners: std::pin::Pin<
        &mut impl Stream<Item = (Session<RemoteDesktop>, SelectionOwnerChanged)>,
    >,
) -> Option<Snapshot> {
    let empty = || {
        MIME_TYPES
            .iter()
            .map(|mime| ((*mime).to_owned(), Vec::new()))
            .collect()
    };
    let first = future::select(owners.next(), Timer::after(OWNER_WAIT)).await;
    let future::Either::Left((Some((_, owner)), _)) = first else {
        return Some(empty());
    };
    if owner.session_is_owner() == Some(true) {
        return None;
    }
    if owner.mime_types().is_empty() {
        return Some(empty());
    }
    let mut saved = Snapshot::new();
    let mut total = 0u64;
    for mime in owner.mime_types() {
        // A type that cannot be read in full would come back damaged.
        let bytes = read_selection(clipboard, session, mime, SNAPSHOT_LIMIT - total).await?;
        total += bytes.len() as u64;
        saved.push((mime.clone(), bytes));
    }
    Some(saved)
}

/// Reads one clipboard type. `None` when it fails, times out or exceeds `limit`.
async fn read_selection(
    clipboard: &Clipboard,
    session: &Session<RemoteDesktop>,
    mime: &str,
    limit: u64,
) -> Option<Vec<u8>> {
    let fd = clipboard.selection_read(session, mime).await.ok()?;
    let file = std::fs::File::from(std::os::fd::OwnedFd::from(fd));
    let reader = async_io::Async::new(file).ok()?;
    let mut bytes = Vec::new();
    let complete = {
        // One byte more than the limit shows that the content is too large.
        let mut limited = reader.take(limit + 1);
        let read = limited.read_to_end(&mut bytes);
        pin_mut!(read);
        matches!(
            future::select(read, Timer::after(READ_TIMEOUT)).await,
            future::Either::Left((Ok(_), _))
        )
    };
    (complete && bytes.len() as u64 <= limit).then_some(bytes)
}

async fn paste(
    remote: &RemoteDesktop,
    clipboard: &Clipboard,
    session: &Session<RemoteDesktop>,
    bytes: &[u8],
    saved: Option<&Snapshot>,
    owners: impl Stream<Item = (Session<RemoteDesktop>, SelectionOwnerChanged)>,
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
    let delivered = std::cell::Cell::new(false);
    let restoring = std::cell::Cell::new(false);
    let restored = std::cell::Cell::new(false);
    let serve = async {
        // Portals send session signals only to the session's owner, and this
        // connection has no other remote desktop session open.
        while let Some((_, mime, serial)) = transfers.next().await {
            let data = if restoring.get() {
                saved
                    .into_iter()
                    .flatten()
                    .find(|(saved_mime, _)| *saved_mime == mime)
                    .map(|(_, data)| data.as_slice())
            } else {
                Some(bytes)
            };
            let written = match (data, clipboard.selection_write(session, serial).await) {
                (Some(data), Ok(fd)) => std::fs::File::from(std::os::fd::OwnedFd::from(fd))
                    .write_all(data)
                    .is_ok(),
                _ => false,
            };
            if written {
                if restoring.get() {
                    restored.set(true);
                } else {
                    delivered.set(true);
                }
            }
            let _ = clipboard
                .selection_write_done(session, serial, written)
                .await;
        }
    };
    let press = async {
        press_ctrl_v(remote, session).await?;
        Timer::after(PASTE_GRACE).await;
        if let Some(saved) = saved
            && !taken_by_another_app(owners)
        {
            restore(clipboard, session, saved, &restoring, &restored).await?;
        }
        Ok::<(), InsertError>(())
    };
    pin_mut!(serve, press);
    match future::select(press, serve).await {
        future::Either::Left((result, _)) => result?,
        future::Either::Right(((), press)) => press.await?,
    }
    // A served transfer shows that a reader got the text, not that the target
    // inserted it. Without one, the paste certainly failed.
    if delivered.get() {
        Ok(())
    } else {
        Err(InsertError::NotDelivered)
    }
}

/// Presses Ctrl+V on the portal's virtual keyboard, with gaps between the keys.
async fn press_ctrl_v(
    remote: &RemoteDesktop,
    session: &Session<RemoteDesktop>,
) -> Result<(), InsertError> {
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
    Ok(())
}

/// True when another app has taken the clipboard since the paste, as the
/// owner reports that arrived in the meantime show. Its content then stays.
fn taken_by_another_app(
    owners: impl Stream<Item = (Session<RemoteDesktop>, SelectionOwnerChanged)>,
) -> bool {
    pin_mut!(owners);
    let mut taken = false;
    while let Some(Some((_, owner))) = owners.next().now_or_never() {
        taken = owner.session_is_owner() == Some(false);
    }
    taken
}

/// Offers the saved content and waits until the desktop has fetched it once.
async fn restore(
    clipboard: &Clipboard,
    session: &Session<RemoteDesktop>,
    saved: &Snapshot,
    restoring: &std::cell::Cell<bool>,
    restored: &std::cell::Cell<bool>,
) -> Result<(), InsertError> {
    let mimes: Vec<&str> = saved.iter().map(|(mime, _)| mime.as_str()).collect();
    restoring.set(true);
    clipboard
        .set_selection(
            session,
            SetSelectionOptions::default().set_mime_types(&mimes),
        )
        .await?;
    let started = std::time::Instant::now();
    while !restored.get() && started.elapsed() < RESTORE_WAIT {
        Timer::after(Duration::from_millis(20)).await;
    }
    Ok(())
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
