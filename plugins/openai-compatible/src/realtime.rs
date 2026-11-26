//! Realtime transcription over the `/v1/realtime` WebSocket, as in the Windows
//! app: one transcription session per dictation, audio as 24 kHz PCM16, no
//! server-side turn detection, and a commit at the end whose transcript is the
//! result.
//!
//! The session opens with `transcription.begin` and takes audio as it arrives,
//! so it also works while a recording is still running. Interim text goes out
//! as `transcription.partial` notifications.

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};
use tw_openai_compatible::server_url::{self, withholds_api_key};
use tw_openai_compatible::{Profile, endpoint};

use crate::batch::{self, Options, Transcript};
use crate::failure::{self, Failure};

pub const SOURCE_RATE: u32 = 16_000;
pub const TARGET_RATE: u32 = 24_000;
// The steps together stay below the host's limit of 30 seconds plus the length
// of the audio, so the plugin reports a slow server before the host stops it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the server has to accept the session settings.
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the server has, besides the length of the audio, to send the
/// final transcript after the commit.
const FINAL_TIME: Duration = Duration::from_secs(10);
/// How often the session thread looks for new audio while it waits for events.
const POLL: Duration = Duration::from_millis(20);
/// Windows refuses larger messages.
const MAX_MESSAGE: usize = 1 << 20;
/// 200 ms of source audio per `input_audio_buffer.append`, as on Windows.
const CHUNK_SAMPLES: usize = SOURCE_RATE as usize / 5;

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

enum Command {
    Audio(Vec<f32>),
    Commit,
}

/// An open session. Dropping it closes the connection.
pub struct Session {
    commands: mpsc::Sender<Command>,
    result: mpsc::Receiver<Result<String, Failure>>,
    thread: Option<JoinHandle<()>>,
    /// Whisper-family sessions take one language and report none back.
    language: Option<String>,
}

impl Session {
    /// Connects and waits until the server has accepted the session settings.
    /// `on_partial` gets the text so far whenever it changes.
    pub fn open(
        profile: &Profile,
        key: Option<&str>,
        options: &Options,
        on_partial: Box<dyn FnMut(&str) + Send>,
    ) -> Result<Self, Failure> {
        if options.translate {
            return Err(Failure::RealtimeTranslation);
        }
        let model = profile.selected_model_id.trim();
        if model.is_empty() {
            return Err(Failure::NotConfigured(failure::NO_TRANSCRIPTION_MODEL));
        }
        let url = endpoint::realtime_url(profile)?;
        let withheld = withholds_api_key(&url, key);
        let mut socket = connect(&url, key).map_err(|f| f.explain_withheld_key(withheld))?;
        let live = profile.uses_live_protocol();
        let hints: Vec<String> = batch::language_hint(options.language.as_deref())
            .map(|l| vec![l.to_owned()])
            .unwrap_or_default();
        let update = session_update(model, live, &hints, options.prompt.as_deref());
        send(&mut socket, update.to_string())?;
        let mut collector = Collector::default();
        wait_until_ready(&mut socket, &mut collector)?;

        let (commands, command_rx) = mpsc::channel();
        let (result_tx, result) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("realtime-session".into())
            .spawn(move || {
                let outcome = run(socket, collector, command_rx, on_partial);
                let _ = result_tx.send(outcome);
            })
            .map_err(failure::network)?;
        Ok(Self {
            commands,
            result,
            thread: Some(thread),
            language: if live { None } else { hints.into_iter().next() },
        })
    }

    pub fn send_audio(&self, samples: Vec<f32>) {
        // A closed session reports its failure in `finish`.
        let _ = self.commands.send(Command::Audio(samples));
    }

    /// Commits the audio, `samples` samples in all, and waits for its final
    /// transcript.
    pub fn finish(mut self, samples: usize) -> Result<Transcript, Failure> {
        let _ = self.commands.send(Command::Commit);
        let limit = FINAL_TIME + batch::audio_length(samples);
        let text = match self.result.recv_timeout(limit) {
            Ok(result) => result?,
            Err(RecvTimeoutError::Timeout) => {
                return Err(Failure::Realtime(
                    "The server did not confirm the final transcript.",
                ));
            }
            Err(RecvTimeoutError::Disconnected) => return Err(interrupted()),
        };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        Ok(Transcript {
            text,
            language: self.language.take(),
            duration_seconds: None,
            no_speech_probability: None,
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Closing the channel ends the thread at its next poll.
        let (closed, _) = mpsc::channel();
        self.commands = closed;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn interrupted() -> Failure {
    Failure::Realtime("Live transcription was interrupted or returned an invalid response.")
}

/// Opens the WebSocket with a connect timeout, the key under the key rule and
/// TLS from the same roots as the HTTP requests.
fn connect(url: &str, key: Option<&str>) -> Result<Socket, Failure> {
    let parts = server_url::parse(url).map_err(|_| Failure::NotConfigured(failure::INVALID_URL))?;
    let port = parts
        .port
        .unwrap_or(if parts.scheme == "wss" { 443 } else { 80 });
    let addresses: Vec<_> = (parts.host, port)
        .to_socket_addrs()
        .map_err(failure::network)?
        .collect();
    let mut last_error = None;
    let mut stream = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(e) => last_error = Some(e),
        }
    }
    let stream = stream.ok_or_else(|| {
        failure::network(last_error.map_or_else(|| "no address".to_owned(), |e| e.to_string()))
    })?;
    stream
        .set_read_timeout(Some(READY_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(READY_TIMEOUT)))
        .and_then(|()| stream.set_nodelay(true))
        .map_err(failure::network)?;

    let mut request = url
        .into_client_request()
        .map_err(|_| Failure::NotConfigured(failure::INVALID_URL))?;
    for (name, value) in endpoint::authentication_headers(url, key) {
        let value = value
            .parse()
            .map_err(|_| Failure::NotConfigured(failure::INVALID_KEY))?;
        request.headers_mut().insert(name, value);
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    match tungstenite::client_tls_with_config(request, stream, Some(config), None) {
        Ok((socket, _)) => Ok(socket),
        Err(tungstenite::HandshakeError::Failure(error)) => Err(from_socket_error(error)),
        Err(tungstenite::HandshakeError::Interrupted(_)) => Err(failure::timeout()),
    }
}

fn from_socket_error(error: tungstenite::Error) -> Failure {
    match error {
        tungstenite::Error::Http(response) => {
            let body = response
                .body()
                .as_deref()
                .map(String::from_utf8_lossy)
                .unwrap_or_default();
            failure::from_status(response.status().as_u16(), &body)
        }
        tungstenite::Error::Io(e)
            if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
        {
            failure::timeout()
        }
        tungstenite::Error::Io(e) => failure::network(e),
        tungstenite::Error::Tls(e) => failure::network(e),
        _ => interrupted(),
    }
}

fn send(socket: &mut Socket, text: String) -> Result<(), Failure> {
    socket
        .send(Message::Text(text.into()))
        .map_err(from_socket_error)
}

fn set_read_timeout(socket: &Socket, timeout: Duration) {
    let stream = match socket.get_ref() {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::Rustls(stream) => stream.get_ref(),
        _ => return,
    };
    let _ = stream.set_read_timeout(Some(timeout));
}

fn wait_until_ready(socket: &mut Socket, collector: &mut Collector) -> Result<(), Failure> {
    let deadline = Instant::now() + READY_TIMEOUT;
    while !collector.session_ready {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Failure::Realtime(
                "The server did not acknowledge the live session settings.",
            ));
        }
        set_read_timeout(socket, left);
        match socket.read() {
            Ok(Message::Text(text)) => {
                collector.apply(text.as_str());
                if let Some(error) = collector.error.take() {
                    return Err(rejected(&error));
                }
            }
            Ok(Message::Close(_)) => return Err(interrupted()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => return Err(from_socket_error(error)),
        }
    }
    Ok(())
}

fn rejected(message: &str) -> Failure {
    Failure::RealtimeRejected(message.to_owned())
}

/// The session thread: sends audio as it comes, reads events in between, and
/// ends with the committed item's transcript.
fn run(
    mut socket: Socket,
    mut collector: Collector,
    commands: mpsc::Receiver<Command>,
    mut on_partial: Box<dyn FnMut(&str) + Send>,
) -> Result<String, Failure> {
    set_read_timeout(&socket, POLL);
    let mut upsampler = Upsampler::default();
    let mut pending: Vec<f32> = Vec::new();
    let mut committed = false;
    let mut last_partial = String::new();
    loop {
        loop {
            match commands.try_recv() {
                Ok(Command::Audio(samples)) if !committed => {
                    pending.extend(samples);
                    while pending.len() >= CHUNK_SAMPLES {
                        let chunk: Vec<f32> = pending.drain(..CHUNK_SAMPLES).collect();
                        send_audio(&mut socket, &upsampler.push(&chunk))?;
                    }
                }
                Ok(Command::Audio(_)) => {}
                Ok(Command::Commit) if !committed => {
                    let mut rest = upsampler.push(&pending);
                    pending.clear();
                    rest.extend(upsampler.finish());
                    send_audio(&mut socket, &rest)?;
                    send(
                        &mut socket,
                        json!({"type": "input_audio_buffer.commit"}).to_string(),
                    )?;
                    committed = true;
                }
                Ok(Command::Commit) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let _ = socket.close(None);
                    return Err(interrupted());
                }
            }
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                collector.apply(text.as_str());
                if let Some(error) = collector.error.take() {
                    let _ = socket.close(None);
                    return Err(rejected(&error));
                }
                let current = collector.current_text();
                if current != last_partial && !current.is_empty() {
                    on_partial(&current);
                    last_partial = current;
                }
                if collector.is_final() {
                    let _ = socket.close(None);
                    return Ok(collector.current_text());
                }
            }
            Ok(Message::Close(_)) => return Err(interrupted()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                // Sends any pong that a ping queued.
                let _ = socket.flush();
            }
            Err(error) => return Err(from_socket_error(error)),
        }
    }
}

fn send_audio(socket: &mut Socket, pcm: &[i16]) -> Result<(), Failure> {
    if pcm.is_empty() {
        return Ok(());
    }
    send(socket, audio_append(pcm).to_string())
}

/// `input_audio_buffer.append` with 24 kHz PCM16 in base64.
pub fn audio_append(pcm: &[i16]) -> Value {
    let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
    json!({
        "type": "input_audio_buffer.append",
        "audio": base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

/// The `session.update` that opens a transcription session. Live Transcribe
/// models take language hints and a prompt; Whisper-family models one language.
pub fn session_update(model: &str, live: bool, hints: &[String], prompt: Option<&str>) -> Value {
    let mut transcription = json!({"model": model});
    if live {
        if !hints.is_empty() {
            transcription["languages"] = json!(hints);
        }
        if let Some(prompt) = prompt.map(str::trim).filter(|p| !p.is_empty()) {
            transcription["prompt"] = json!(prompt);
        }
    } else if let Some(language) = hints.first() {
        transcription["language"] = json!(language);
    }
    json!({
        "type": "session.update",
        "session": {
            "type": "transcription",
            "audio": {
                "input": {
                    "format": {"type": "audio/pcm", "rate": TARGET_RATE},
                    "transcription": transcription,
                    "turn_detection": null,
                }
            }
        }
    })
}

const UNIDENTIFIED_ITEM: &str = "__unidentified_item__";

/// Collects the transcript from the session's events, as the Windows app's
/// collector does: completed items in order, then items still in progress.
#[derive(Default)]
pub struct Collector {
    completed_order: Vec<String>,
    completed: HashMap<String, String>,
    deltas: HashMap<String, String>,
    committed_item: Option<String>,
    completed_items: HashSet<String>,
    pub session_ready: bool,
    pub error: Option<String>,
}

impl Collector {
    pub fn apply(&mut self, event: &str) {
        let Ok(event) = serde_json::from_str::<Value>(event) else {
            self.error = Some("invalid event".into());
            return;
        };
        let item = event["item_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .unwrap_or(UNIDENTIFIED_ITEM)
            .to_owned();
        match event["type"].as_str().unwrap_or_default() {
            "conversation.item.input_audio_transcription.delta" => {
                if !self.completed.contains_key(&item) {
                    let delta = event["delta"].as_str().unwrap_or_default();
                    self.deltas.entry(item).or_default().push_str(delta);
                }
            }
            "conversation.item.input_audio_transcription.completed" => {
                self.completed_items.insert(item.clone());
                if !self.completed.contains_key(&item) {
                    let transcript = event["transcript"].as_str().unwrap_or_default().trim();
                    self.completed_order.push(item.clone());
                    self.completed.insert(item.clone(), transcript.to_owned());
                    self.deltas.remove(&item);
                }
            }
            "input_audio_buffer.committed" => self.committed_item = Some(item),
            "session.updated" | "transcription_session.updated" => self.session_ready = true,
            "conversation.item.input_audio_transcription.failed" | "error" => {
                self.error = Some(error_message(&event));
            }
            _ => {}
        }
    }

    /// The committed item has its final transcript.
    pub fn is_final(&self) -> bool {
        self.committed_item
            .as_ref()
            .is_some_and(|item| self.completed_items.contains(item))
    }

    pub fn current_text(&self) -> String {
        let mut parts: Vec<&str> = self
            .completed_order
            .iter()
            .filter_map(|id| self.completed.get(id).map(String::as_str))
            .collect();
        let mut open: Vec<(&String, &String)> = self
            .deltas
            .iter()
            .filter(|(id, text)| !self.completed.contains_key(*id) && !text.trim().is_empty())
            .collect();
        open.sort_by_key(|(id, _)| *id);
        parts.extend(open.into_iter().map(|(_, text)| text.as_str()));
        parts.join(" ").trim().to_owned()
    }
}

fn error_message(event: &Value) -> String {
    let error = &event["error"];
    error["message"]
        .as_str()
        .or_else(|| error["type"].as_str())
        .or_else(|| error.as_str())
        .or_else(|| event["message"].as_str())
        .unwrap_or("realtime transcription failed")
        .to_owned()
}

/// Linear 16 to 24 kHz conversion that gives, however the audio is split, the
/// same samples as the Windows app's conversion of the whole recording.
#[derive(Default)]
pub struct Upsampler {
    /// Source samples seen so far.
    seen: u64,
    /// The last of them.
    last: i16,
    /// The next output sample.
    next: u64,
}

impl Upsampler {
    pub fn push(&mut self, samples: &[f32]) -> Vec<i16> {
        let chunk: Vec<i16> = pcm16(samples).collect();
        let start = self.seen;
        let total = start + chunk.len() as u64;
        let sample = |index: u64, last: i16| -> f64 {
            if index < start {
                f64::from(last)
            } else {
                f64::from(chunk[(index - start) as usize])
            }
        };
        let mut out = Vec::with_capacity(chunk.len() * 3 / 2 + 1);
        loop {
            let lower = self.next * 2 / 3;
            if lower + 1 >= total {
                break;
            }
            let fraction = (self.next * 2 % 3) as f64 / 3.0;
            let (a, b) = (sample(lower, self.last), sample(lower + 1, self.last));
            out.push(round(a + (b - a) * fraction));
            self.next += 1;
        }
        if let Some(last) = chunk.last() {
            self.last = *last;
        }
        self.seen = total;
        out
    }

    /// The samples at the end that need no later audio.
    pub fn finish(&mut self) -> Vec<i16> {
        if self.seen == 0 {
            return Vec::new();
        }
        let count = (self.seen * 3).div_ceil(2);
        let mut out = Vec::new();
        while self.next < count {
            out.push(self.last);
            self.next += 1;
        }
        out
    }
}

/// 16-bit samples, rounded like the WAV files of the batch upload.
pub fn pcm16(samples: &[f32]) -> impl Iterator<Item = i16> + '_ {
    samples.iter().map(|&sample| {
        let sample = if sample.is_finite() { sample } else { 0.0 };
        let scaled = (f64::from(sample.clamp(-1.0, 1.0)) * 32768.0).round_ties_even();
        scaled.clamp(-32768.0, 32767.0) as i16
    })
}

fn round(value: f64) -> i16 {
    value
        .round()
        .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Windows app's conversion of a whole recording.
    fn reference(source: &[i16]) -> Vec<i16> {
        let n = source.len();
        let count = ((n as f64) * 1.5).round().max(1.0) as usize;
        (0..count)
            .map(|t| {
                let position = t as f64 * 16_000.0 / 24_000.0;
                let lower = (position.floor() as usize).min(n - 1);
                let upper = (lower + 1).min(n - 1);
                let fraction = position - lower as f64;
                let (a, b) = (f64::from(source[lower]), f64::from(source[upper]));
                round(a + (b - a) * fraction)
            })
            .collect()
    }

    #[test]
    fn upsamples_like_the_windows_app_however_the_audio_is_split() {
        let samples: Vec<f32> = (0..1001).map(|i| ((i as f32) * 0.37).sin() * 0.8).collect();
        let pcm: Vec<i16> = pcm16(&samples).collect();
        let expected = reference(&pcm);
        for split in [1, 2, 3, 7, 160, 1600, 5000] {
            let mut upsampler = Upsampler::default();
            let mut out = Vec::new();
            for chunk in samples.chunks(split) {
                out.extend(upsampler.push(chunk));
            }
            out.extend(upsampler.finish());
            assert_eq!(out.len(), expected.len(), "split {split}");
            for (i, (a, b)) in out.iter().zip(&expected).enumerate() {
                assert!((a - b).abs() <= 1, "split {split}, sample {i}: {a} vs {b}");
            }
        }
        let mut empty = Upsampler::default();
        assert!(empty.push(&[]).is_empty() && empty.finish().is_empty());
    }

    #[test]
    fn session_update_follows_the_model_family() {
        let hints = vec!["de".to_owned()];
        let live = session_update("gpt-live-transcribe", true, &hints, Some("Names: Ada"));
        let input = &live["session"]["audio"]["input"];
        assert_eq!(live["type"], "session.update");
        assert_eq!(live["session"]["type"], "transcription");
        assert_eq!(input["format"], json!({"type": "audio/pcm", "rate": 24000}));
        assert_eq!(input["turn_detection"], Value::Null);
        assert_eq!(
            input["transcription"],
            json!({"model": "gpt-live-transcribe", "languages": ["de"], "prompt": "Names: Ada"})
        );
        let whisper = session_update("gpt-realtime-whisper", false, &hints, Some("ignored"));
        assert_eq!(
            whisper["session"]["audio"]["input"]["transcription"],
            json!({"model": "gpt-realtime-whisper", "language": "de"})
        );
        let bare = session_update("m", true, &[], None);
        assert_eq!(
            bare["session"]["audio"]["input"]["transcription"],
            json!({"model": "m"})
        );
    }

    #[test]
    fn appends_base64_pcm() {
        let append = audio_append(&[1, -2]);
        assert_eq!(append["type"], "input_audio_buffer.append");
        assert_eq!(append["audio"], "AQD+/w==");
    }

    fn event(value: Value) -> String {
        value.to_string()
    }

    #[test]
    fn waits_for_the_committed_items_final_transcript() {
        let mut c = Collector::default();
        c.apply(&event(json!({"type": "session.updated"})));
        assert!(c.session_ready);
        c.apply(&event(json!({"type": "conversation.item.input_audio_transcription.delta", "item_id": "a", "delta": "Hel"})));
        c.apply(&event(json!({"type": "conversation.item.input_audio_transcription.delta", "item_id": "a", "delta": "lo"})));
        assert_eq!(c.current_text(), "Hello");
        assert!(!c.is_final());
        c.apply(&event(
            json!({"type": "input_audio_buffer.committed", "item_id": "a"}),
        ));
        assert!(!c.is_final());
        c.apply(&event(json!({"type": "conversation.item.input_audio_transcription.completed", "item_id": "a", "transcript": " Hello. "})));
        assert!(c.is_final());
        assert_eq!(c.current_text(), "Hello.");
        // A repeated final changes nothing.
        c.apply(&event(json!({"type": "conversation.item.input_audio_transcription.completed", "item_id": "a", "transcript": "Other"})));
        c.apply(&event(json!({"type": "conversation.item.input_audio_transcription.delta", "item_id": "a", "delta": "late"})));
        assert_eq!(c.current_text(), "Hello.");
    }

    #[test]
    fn missing_item_ids_still_complete_the_session_in_either_order() {
        for completed_first in [false, true] {
            let mut c = Collector::default();
            let committed = event(json!({"type": "input_audio_buffer.committed"}));
            let completed = event(
                json!({"type": "conversation.item.input_audio_transcription.completed", "transcript": "Done"}),
            );
            if completed_first {
                c.apply(&completed);
                c.apply(&committed);
            } else {
                c.apply(&committed);
                c.apply(&completed);
            }
            assert!(c.is_final());
            assert_eq!(c.current_text(), "Done");
        }
        let mut deltas = Collector::default();
        for delta in ["One", " two"] {
            deltas.apply(&event(json!({"type": "conversation.item.input_audio_transcription.delta", "delta": delta})));
        }
        assert_eq!(deltas.current_text(), "One two");
    }

    #[test]
    fn keeps_the_servers_error() {
        let mut c = Collector::default();
        c.apply(&event(json!({"type": "error", "error": {"type": "invalid_request_error", "message": "Unknown model"}})));
        assert_eq!(c.error.as_deref(), Some("Unknown model"));
        let mut failed = Collector::default();
        failed.apply(&event(json!({"type": "conversation.item.input_audio_transcription.failed", "error": {"type": "server_error"}})));
        assert_eq!(failed.error.as_deref(), Some("server_error"));
        let mut garbage = Collector::default();
        garbage.apply("not json");
        assert!(garbage.error.is_some());
    }
}
