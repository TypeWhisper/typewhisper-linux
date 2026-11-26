//! OpenAI Whisper through whisper.cpp as a TypeWhisper plugin process, like the
//! Windows app's whisper.cpp plugin. It runs on the CPU.
//!
//! The plugin keeps its models in the data folder the host passes to `initialize`.
//! `models.download` fetches one GGML file, checks its size and SHA-256, and
//! reports progress as `models.progress` notifications. Only one model is loaded
//! at a time, as the larger ones take more than a gigabyte of memory.

mod models;

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Instant;

use models::{MODELS, Model};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tw_plugin_protocol::{Frame, PROTOCOL_VERSION, read_frame, write_frame};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

const PLUGIN_ID: &str = "com.typewhisper.whisper-cpp";
const SAMPLE_RATE: usize = 16_000;
/// Whisper hears too little in a fraction of a second, so shorter recordings
/// get silence up to one second.
const MIN_SAMPLES: usize = SAMPLE_RATE;
/// Whisper writes words even into silence, such as "you" or "See you next
/// time!". As on Windows, a result whose every segment is more likely than this
/// to hold no speech counts as silence.
const NO_SPEECH: f32 = 0.8;

type RpcResult = Result<Value, (i64, String)>;

struct Plugin {
    data_dir: PathBuf,
    loaded: Option<Loaded>,
    pending: Option<Pending>,
}

struct Loaded {
    model: &'static Model,
    /// Holds the model and the buffers of one transcription, reused for the next.
    state: WhisperState,
    threads: i32,
}

struct Pending {
    request_id: Value,
    language: Option<String>,
    samples: Vec<f32>,
}

impl Plugin {
    fn model_path(&self, model: &Model) -> PathBuf {
        self.data_dir.join("models").join(model.file())
    }

    /// The file is there and has the size of the checked download. A full
    /// checksum of up to 1.6 GB would make every start slow.
    fn installed(&self, model: &Model) -> bool {
        fs::metadata(self.model_path(model)).is_ok_and(|m| m.len() == model.size_bytes)
    }

    fn handle(&mut self, method: &str, params: &Value, out: &mut impl Write) -> RpcResult {
        match method {
            "initialize" => {
                if params["protocolVersion"].as_u64() != Some(u64::from(PROTOCOL_VERSION)) {
                    return Err((-32000, "unsupported protocol version".into()));
                }
                if let Some(dir) = params["dataDir"].as_str() {
                    self.data_dir = PathBuf::from(dir);
                }
                let models: Vec<Value> = MODELS
                    .iter()
                    .map(|m| {
                        json!({
                            "id": m.id,
                            "name": m.name,
                            "englishOnly": m.english_only(),
                            "recommended": m.recommended(),
                            "sizeBytes": m.size_bytes,
                            "installed": self.installed(m)
                        })
                    })
                    .collect();
                Ok(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "pluginId": PLUGIN_ID,
                    "capabilities": ["transcription"],
                    "models": models,
                    // Whisper encodes every 30-second window in full, however little of
                    // it is speech. Large V3 Turbo took up to 72 s for a 4-second clip on
                    // a six-core ARM VM, so the host's default would stop it.
                    "transcriptionTimeout": {"baseSeconds": 180, "audioFactor": 3}
                }))
            }
            "models.download" => {
                let model = requested_model(params)?;
                // `force` replaces a file that exists but does not load. The old file
                // stays until the new download has passed its checksum.
                let force = params["force"].as_bool().unwrap_or(false);
                if force || !self.installed(model) {
                    download_model(model, &self.model_path(model), out)
                        .map_err(|e| (-32010, format!("Download failed: {e}")))?;
                }
                Ok(json!({"installed": self.installed(model)}))
            }
            "transcription.loadModel" => {
                let started = Instant::now();
                let model = requested_model(params)?;
                let threads = params["threads"].as_i64().unwrap_or(4).clamp(1, 64) as i32;
                // Frees the memory of the previous model before the next one loads.
                self.loaded = None;
                let failed = || {
                    (
                        -32001,
                        "The model could not be loaded. Download it again.".to_string(),
                    )
                };
                if !self.installed(model) {
                    return Err(failed());
                }
                let mut context_params = WhisperContextParameters::default();
                context_params.use_gpu(false);
                let state = WhisperContext::new_with_params(self.model_path(model), context_params)
                    .and_then(|context| context.create_state())
                    .map_err(|_| failed())?;
                self.loaded = Some(Loaded {
                    model,
                    state,
                    threads,
                });
                Ok(json!({"loadMs": started.elapsed().as_millis() as u64}))
            }
            "transcription.begin" => {
                self.pending = Some(Pending {
                    request_id: params["requestId"].clone(),
                    language: params["language"].as_str().map(str::to_owned),
                    samples: Vec::new(),
                });
                Ok(json!({}))
            }
            "transcription.end" => {
                let loaded = self
                    .loaded
                    .as_mut()
                    .ok_or((-32002, "no model loaded".to_string()))?;
                let pending = self
                    .pending
                    .take()
                    .ok_or((-32003, "no open request".to_string()))?;
                let started = Instant::now();
                let (text, language) =
                    transcribe(loaded, &pending.samples, pending.language.as_deref())
                        .map_err(|e| (-32004, format!("Transcription failed: {e:?}")))?;
                Ok(json!({
                    "requestId": pending.request_id,
                    "text": text,
                    "language": language,
                    "audioMs": (pending.samples.len() * 1000 / SAMPLE_RATE) as u64,
                    "decodeMs": started.elapsed().as_millis() as u64
                }))
            }
            "shutdown" => Ok(json!({})),
            other => Err((-32601, format!("unknown method {other}"))),
        }
    }
}

/// The model named in `modelId`, or the recommended one when the host names none.
fn requested_model(params: &Value) -> Result<&'static Model, (i64, String)> {
    match params["modelId"].as_str() {
        Some(id) => models::find(id).ok_or((-32602, format!("unknown model {id}"))),
        None => Ok(MODELS
            .iter()
            .find(|m| m.recommended())
            .expect("a recommended model")),
    }
}

/// Runs the whole recording through Whisper. Returns the text and the language
/// Whisper heard.
fn transcribe(
    loaded: &mut Loaded,
    samples: &[f32],
    language: Option<&str>,
) -> Result<(String, Option<String>), whisper_rs::WhisperError> {
    // Greedy decoding with whisper.cpp's defaults, as Whisper.net uses on Windows.
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 5 });
    params.set_n_threads(loaded.threads);
    // Every dictation stands alone; the text of the last one is no prompt.
    params.set_no_context(true);
    // No language means detection; the English models need none.
    let language = if loaded.model.english_only() {
        Some("en")
    } else {
        language.filter(|l| !l.is_empty() && *l != "auto")
    };
    params.set_language(language);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    let mut padded = Vec::new();
    let audio = if samples.len() < MIN_SAMPLES {
        padded.extend_from_slice(samples);
        padded.resize(MIN_SAMPLES, 0.0);
        &padded
    } else {
        samples
    };
    let state = &mut loaded.state;
    state.full(params, audio)?;

    // Whisper puts its own spaces between segments, and none in Chinese or Japanese.
    let mut text = String::new();
    let mut least_no_speech = 1.0f32;
    for segment in state.as_iter() {
        text.push_str(&segment.to_str_lossy()?);
        least_no_speech = least_no_speech.min(segment.no_speech_probability());
    }
    let heard = whisper_rs::get_lang_str(state.full_lang_id_from_state()).map(str::to_owned);
    let text = text.trim();
    if least_no_speech > NO_SPEECH || is_annotation(text) {
        return Ok((String::new(), heard));
    }
    Ok((text.to_owned(), heard))
}

/// A result such as `[BLANK_AUDIO]` or `[Music]`, which Whisper writes for
/// sounds that are no speech.
fn is_annotation(text: &str) -> bool {
    text.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .is_some_and(|inner| !inner.contains(['[', ']']))
}

fn notify(out: &mut impl Write, method: &str, params: Value) -> io::Result<()> {
    write_frame(
        out,
        &Frame::Json(json!({"jsonrpc": "2.0", "method": method, "params": params})),
    )
}

fn report_progress(out: &mut impl Write, model: &Model, received: u64) {
    let _ = notify(
        out,
        "models.progress",
        json!({"modelId": model.id, "receivedBytes": received, "totalBytes": model.size_bytes}),
    );
}

fn download_model(model: &Model, target: &Path, out: &mut impl Write) -> Result<(), String> {
    let folder = target.parent().expect("models folder");
    fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    let partial = folder.join(format!(".{}.part", model.file()));

    let response = ureq::get(&model.url()).call().map_err(|e| e.to_string())?;
    let mut body = response.into_body().into_reader();
    let mut file = File::create(&partial).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    let (mut received, mut reported) = (0u64, 0u64);
    loop {
        let n = body.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        file.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
        received += n as u64;
        if received > model.size_bytes {
            break;
        }
        if received - reported >= model.size_bytes / 100 {
            reported = received;
            report_progress(out, model, received);
        }
    }
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if received != model.size_bytes || digest != model.sha256 {
        let _ = fs::remove_file(&partial);
        return Err("the downloaded file does not match the expected checksum".into());
    }
    report_progress(out, model, received);
    fs::rename(&partial, target).map_err(|e| e.to_string())
}

/// Passes whisper.cpp's warnings and errors to standard error. Its information
/// messages describe every model load and transcription in a dozen lines.
struct WarningsToStderr;

impl log::Log for WarningsToStderr {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("whisper.cpp: {}", record.args());
        }
    }

    fn flush(&self) {}
}

/// The protocol owns standard output. whisper.cpp writes its log to standard
/// error, but a stray `printf` in the C++ code would corrupt a frame, so the
/// frames go to a copy of the original output and file descriptor 1 is pointed
/// at standard error.
fn protocol_output() -> io::Result<File> {
    // SAFETY: dup and dup2 only duplicate descriptors this process owns; the
    // new descriptor from dup is owned by the returned file alone.
    unsafe {
        let copy = libc::dup(libc::STDOUT_FILENO);
        if copy < 0 || libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(File::from(OwnedFd::from_raw_fd(copy)))
    }
}

fn main() -> io::Result<()> {
    if log::set_logger(&WarningsToStderr).is_ok() {
        log::set_max_level(log::LevelFilter::Warn);
    }
    whisper_rs::install_logging_hooks();
    let mut input = io::stdin().lock();
    let mut output = BufWriter::new(protocol_output()?);
    let mut plugin = Plugin {
        data_dir: PathBuf::from("."),
        loaded: None,
        pending: None,
    };

    while let Some(frame) = read_frame(&mut input)? {
        let message = match frame {
            Frame::Pcm(samples) => {
                if let Some(pending) = plugin.pending.as_mut() {
                    pending.samples.extend(samples);
                }
                continue;
            }
            Frame::Json(message) => message,
        };
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message["method"].as_str().unwrap_or_default().to_owned();
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let reply = match plugin.handle(&method, &params, &mut output) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, text)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": text}})
            }
        };
        write_frame(&mut output, &Frame::Json(reply))?;
        if method == "shutdown" {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_annotations_for_sounds() {
        assert!(is_annotation("[BLANK_AUDIO]"));
        assert!(is_annotation("[Music]"));
        assert!(!is_annotation("Set [x] to 3"));
        assert!(!is_annotation("[a] and [b]"));
        assert!(!is_annotation("Hello."));
        assert!(!is_annotation(""));
    }
}
