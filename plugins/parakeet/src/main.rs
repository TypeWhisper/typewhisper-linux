//! NVIDIA Parakeet TDT 0.6B v3 (int8, sherpa-onnx export) as a TypeWhisper plugin process.
//!
//! The plugin keeps its model in the data folder the host passes to `initialize`.
//! `models.download` fetches the sherpa-onnx release archive, checks its SHA-256
//! and unpacks it, reporting progress as `models.progress` notifications.

mod chunks;

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig};
use tw_plugin_protocol::{Frame, PROTOCOL_VERSION, read_frame, write_frame};

const PLUGIN_ID: &str = "com.typewhisper.parakeet";
const MODEL_ID: &str = "parakeet-tdt-0.6b-v3-int8";
const MODEL_URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2";
const MODEL_SHA256: &str = "1495e91b59d7fc53ddca91f3431758ec99f8fa253fdb0e1e851be0f72b14b0f9";
const MODEL_BYTES: u64 = 487_211_443;
const ARCHIVE_ROOT: &str = "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8";
const MODEL_FILES: &[&str] = &[
    "encoder.int8.onnx",
    "decoder.int8.onnx",
    "joiner.int8.onnx",
    "tokens.txt",
];

type RpcResult = Result<Value, (i64, String)>;

struct Plugin {
    data_dir: PathBuf,
    recognizer: Option<OfflineRecognizer>,
    pending: Option<(Value, Vec<f32>)>,
}

impl Plugin {
    fn model_dir(&self) -> PathBuf {
        self.data_dir.join("models").join(MODEL_ID)
    }

    fn model_installed(&self) -> bool {
        MODEL_FILES
            .iter()
            .all(|f| self.model_dir().join(f).is_file())
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
                Ok(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "pluginId": PLUGIN_ID,
                    "capabilities": ["transcription"],
                    "models": [{
                        "id": MODEL_ID,
                        "name": "Parakeet TDT 0.6B v3",
                        "languages": "25 European languages",
                        "sizeBytes": MODEL_BYTES,
                        "installed": self.model_installed()
                    }]
                }))
            }
            "models.download" => {
                // `force` replaces files that exist but do not load. The old files stay
                // until the new download has passed its checksum.
                let force = params["force"].as_bool().unwrap_or(false);
                if force || !self.model_installed() {
                    download_model(&self.data_dir, out)
                        .map_err(|e| (-32010, format!("Download failed: {e}")))?;
                }
                Ok(json!({"installed": self.model_installed()}))
            }
            "transcription.loadModel" => {
                let started = Instant::now();
                let dir = params["path"]
                    .as_str()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.model_dir());
                let threads = params["threads"].as_i64().unwrap_or(4) as i32;
                self.recognizer = load(&dir, threads);
                match self.recognizer {
                    Some(_) => Ok(json!({"loadMs": started.elapsed().as_millis() as u64})),
                    None => Err((
                        -32001,
                        "The model could not be loaded. Download it again.".into(),
                    )),
                }
            }
            "transcription.begin" => {
                self.pending = Some((params["requestId"].clone(), Vec::new()));
                Ok(json!({}))
            }
            "transcription.end" => {
                let recognizer = self
                    .recognizer
                    .as_ref()
                    .ok_or((-32002, "no model loaded".to_string()))?;
                let (request_id, samples) = self
                    .pending
                    .take()
                    .ok_or((-32003, "no open request".to_string()))?;
                let started = Instant::now();
                let text = chunks::transcribe(&samples, |chunk| {
                    let stream = recognizer.create_stream();
                    stream.accept_waveform(16_000, chunk);
                    recognizer.decode(&stream);
                    stream.get_result().map(|r| r.text).unwrap_or_default()
                });
                Ok(json!({
                    "requestId": request_id,
                    "text": text.trim(),
                    "audioMs": samples.len() as u64 * 1000 / 16_000,
                    "decodeMs": started.elapsed().as_millis() as u64
                }))
            }
            "shutdown" => Ok(json!({})),
            other => Err((-32601, format!("unknown method {other}"))),
        }
    }
}

fn load(model_dir: &Path, threads: i32) -> Option<OfflineRecognizer> {
    let file = |name: &str| Some(model_dir.join(name).to_string_lossy().into_owned());
    let mut config = OfflineRecognizerConfig::default();
    config.model_config.transducer = OfflineTransducerModelConfig {
        encoder: file("encoder.int8.onnx"),
        decoder: file("decoder.int8.onnx"),
        joiner: file("joiner.int8.onnx"),
    };
    config.model_config.tokens = file("tokens.txt");
    config.model_config.model_type = Some("nemo_transducer".into());
    config.model_config.num_threads = threads;
    OfflineRecognizer::create(&config)
}

fn notify(out: &mut impl Write, method: &str, params: Value) -> io::Result<()> {
    write_frame(
        out,
        &Frame::Json(json!({"jsonrpc": "2.0", "method": method, "params": params})),
    )
}

fn download_model(data_dir: &Path, out: &mut impl Write) -> Result<(), String> {
    let models = data_dir.join("models");
    fs::create_dir_all(&models).map_err(|e| e.to_string())?;
    let archive = models.join(format!(".{MODEL_ID}.tar.bz2.part"));

    let response = ureq::get(MODEL_URL).call().map_err(|e| e.to_string())?;
    let mut body = response.into_body().into_reader();
    let mut file = fs::File::create(&archive).map_err(|e| e.to_string())?;
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
        if received - reported >= MODEL_BYTES / 100 {
            reported = received;
            let _ = notify(
                out,
                "models.progress",
                json!({"modelId": MODEL_ID, "receivedBytes": received, "totalBytes": MODEL_BYTES}),
            );
        }
    }
    drop(file);
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if digest != MODEL_SHA256 {
        let _ = fs::remove_file(&archive);
        return Err("the downloaded file does not match the expected checksum".into());
    }

    let _ = notify(
        out,
        "models.progress",
        json!({"modelId": MODEL_ID, "receivedBytes": received, "totalBytes": MODEL_BYTES, "unpacking": true}),
    );
    let staging = models.join(format!(".{MODEL_ID}.unpack"));
    let _ = fs::remove_dir_all(&staging);
    let reader = bzip2::read::BzDecoder::new(fs::File::open(&archive).map_err(|e| e.to_string())?);
    // tar's unpack refuses entries that would leave the target folder.
    tar::Archive::new(reader)
        .unpack(&staging)
        .map_err(|e| e.to_string())?;
    let target = models.join(MODEL_ID);
    let _ = fs::remove_dir_all(&target);
    fs::rename(staging.join(ARCHIVE_ROOT), &target).map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(target.join("test_wavs"));
    let _ = fs::remove_dir_all(&staging);
    let _ = fs::remove_file(&archive);
    Ok(())
}

fn main() -> io::Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut plugin = Plugin {
        data_dir: PathBuf::from("."),
        recognizer: None,
        pending: None,
    };

    while let Some(frame) = read_frame(&mut input)? {
        let message = match frame {
            Frame::Pcm(samples) => {
                if let Some((_, buffer)) = plugin.pending.as_mut() {
                    buffer.extend(samples);
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
