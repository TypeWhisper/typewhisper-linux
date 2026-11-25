//! The clipboard through the Wayland data-control protocol, offered by KDE
//! Plasma and wlroots compositors but not by GNOME.
//!
//! On KDE Plasma, other apps cannot paste a selection that a RemoteDesktop
//! portal session set, and the clipboard is empty once that session closes. So
//! where data-control exists, TypeWhisper sets and reads the clipboard itself and
//! keeps serving it until another app copies something. The calls block on
//! Wayland round trips and run on their own threads.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_io::Timer;
use futures_util::future;
use wl_clipboard_rs::{copy, paste};

/// The clipboard content, one entry per MIME type, in the order offered.
pub type Snapshot = Vec<(String, Vec<u8>)>;

/// Runs blocking work on a new thread. `None` if the thread panicked.
async fn unblock<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = sender.send_blocking(work());
    });
    receiver.recv().await.ok()
}

/// True when the compositor offers data-control. Checked once per run.
pub async fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    if let Some(known) = AVAILABLE.get() {
        return *known;
    }
    let found = unblock(|| {
        matches!(
            paste::get_mime_types(paste::ClipboardType::Regular, paste::Seat::Unspecified),
            Ok(_)
                | Err(paste::Error::ClipboardEmpty)
                | Err(paste::Error::NoSeats)
                | Err(paste::Error::NoMimeType)
        )
    })
    .await
    .unwrap_or(false);
    *AVAILABLE.get_or_init(|| found)
}

/// Reads every type the clipboard offers. `Some(empty)` for an empty clipboard,
/// `None` when a type cannot be read in full within `timeout` and `limit` bytes.
pub async fn snapshot(limit: u64, timeout: Duration) -> Option<Snapshot> {
    let deadline = std::time::Instant::now() + timeout;
    unblock(move || {
        let regular = paste::ClipboardType::Regular;
        let mimes = match paste::get_mime_types_ordered(regular, paste::Seat::Unspecified) {
            Ok(mimes) => mimes,
            Err(paste::Error::ClipboardEmpty | paste::Error::NoSeats) => return Some(Vec::new()),
            Err(_) => return None,
        };
        let mut saved = Snapshot::new();
        let mut total = 0u64;
        for mime in mimes {
            let (pipe, _) = paste::get_contents(
                regular,
                paste::Seat::Unspecified,
                paste::MimeType::Specific(&mime),
            )
            .ok()?;
            let bytes = read_until(pipe, limit - total, deadline)?;
            total += bytes.len() as u64;
            if total > limit {
                return None;
            }
            saved.push((mime, bytes));
        }
        Some(saved)
    })
    .await
    .flatten()
}

/// Reads a transfer pipe to its end. `None` when it holds more than `limit`
/// bytes or does not finish by `deadline`; the pipe is closed either way, so an
/// owner that never finishes cannot keep this thread or its descriptor.
fn read_until(
    pipe: impl Into<std::os::fd::OwnedFd>,
    limit: u64,
    deadline: std::time::Instant,
) -> Option<Vec<u8>> {
    use futures_util::AsyncReadExt;
    let file = std::fs::File::from(pipe.into());
    let reader = async_io::Async::new(file).ok()?;
    async_io::block_on(async {
        let mut bytes = Vec::new();
        let complete = {
            // One byte more than the limit shows that the content is too large.
            let mut limited = reader.take(limit + 1);
            let read = limited.read_to_end(&mut bytes);
            futures_util::pin_mut!(read);
            matches!(
                future::select(read, Timer::at(deadline)).await,
                future::Either::Left((Ok(_), _))
            )
        };
        (complete && bytes.len() as u64 <= limit).then_some(bytes)
    })
}

/// Puts text on the clipboard. See [`offer`].
pub async fn offer_text(text: &str, sensitive: bool) -> Result<Arc<AtomicBool>, String> {
    let source = copy::MimeSource {
        source: copy::Source::Bytes(text.as_bytes().into()),
        mime_type: copy::MimeType::Text,
    };
    // The usual text types, including text/plain;charset=utf-8, are added.
    offer(vec![source], false, sensitive).await
}

/// Puts a snapshot back on the clipboard with exactly its MIME types.
pub async fn offer_snapshot(snapshot: Snapshot) -> Result<Arc<AtomicBool>, String> {
    let sources = snapshot
        .into_iter()
        .map(|(mime, bytes)| copy::MimeSource {
            source: copy::Source::Bytes(bytes.into()),
            mime_type: copy::MimeType::Specific(mime),
        })
        .collect();
    offer(sources, true, false).await
}

/// Sets the clipboard and returns once it is set. A thread keeps serving the
/// content until another app copies something; the returned flag turns true
/// then. `exact_types` offers only the given MIME types; `sensitive` asks
/// clipboard managers such as Klipper not to keep the content.
async fn offer(
    sources: Vec<copy::MimeSource>,
    exact_types: bool,
    sensitive: bool,
) -> Result<Arc<AtomicBool>, String> {
    let replaced = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&replaced);
    let (ready_sender, ready) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let mut options = copy::Options::new();
        // Foreground means serving on this thread, which prepare_copy requires.
        options
            .foreground(true)
            .omit_additional_text_mime_types(exact_types)
            .sensitive(sensitive);
        match options.prepare_copy_multi(sources) {
            Ok(prepared) => {
                let _ = ready_sender.send_blocking(Ok(()));
                let _ = prepared.serve();
            }
            Err(error) => {
                let _ = ready_sender.send_blocking(Err(error.to_string()));
            }
        }
        flag.store(true, Ordering::Release);
    });
    ready
        .recv()
        .await
        .map_err(|_| "the clipboard thread stopped".to_owned())??;
    Ok(replaced)
}

/// Empties the clipboard.
pub async fn clear() {
    let _ = unblock(|| copy::clear(copy::ClipboardType::Regular, copy::Seat::All)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Instant;

    #[test]
    fn reads_a_finished_transfer() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(b"hello").unwrap();
        drop(writer);
        let deadline = Instant::now() + Duration::from_secs(2);
        assert_eq!(
            read_until(reader, 100, deadline).as_deref(),
            Some(&b"hello"[..])
        );
    }

    #[test]
    fn gives_up_on_a_transfer_that_never_ends() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(b"partial").unwrap();
        let started = Instant::now();
        let deadline = started + Duration::from_millis(100);
        assert_eq!(read_until(reader, 100, deadline), None);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(writer);
    }

    #[test]
    fn rejects_content_over_the_limit() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(b"0123456789").unwrap();
        drop(writer);
        let deadline = Instant::now() + Duration::from_secs(2);
        assert_eq!(read_until(reader, 5, deadline), None);
    }
}
