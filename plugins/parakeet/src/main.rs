//! NVIDIA Parakeet TDT 0.6B v3 (int8, sherpa-onnx export) as a TypeWhisper plugin process.
//! Model download and selection are not implemented yet; `transcription.loadModel`
//! takes the folder of an unpacked sherpa-onnx Parakeet model.

use std::io;
use std::time::Instant;

use serde_json::{Value, json};
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig};
use tw_plugin_protocol::{Frame, PROTOCOL_VERSION, read_frame, write_frame};

const PLUGIN_ID: &str = "com.typewhisper.parakeet";
const MODEL_ID: &str = "parakeet-tdt-0.6b-v3-int8";

fn load(model_dir: &str, threads: i32) -> Option<OfflineRecognizer> {
    let mut config = OfflineRecognizerConfig::default();
    config.model_config.transducer = OfflineTransducerModelConfig {
        encoder: Some(format!("{model_dir}/encoder.int8.onnx")),
        decoder: Some(format!("{model_dir}/decoder.int8.onnx")),
        joiner: Some(format!("{model_dir}/joiner.int8.onnx")),
    };
    config.model_config.tokens = Some(format!("{model_dir}/tokens.txt"));
    config.model_config.model_type = Some("nemo_transducer".into());
    config.model_config.num_threads = threads;
    OfflineRecognizer::create(&config)
}

struct Plugin {
    recognizer: Option<OfflineRecognizer>,
    pending: Option<(Value, Vec<f32>)>,
}

impl Plugin {
    fn handle(&mut self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => {
                if params["protocolVersion"].as_u64() != Some(u64::from(PROTOCOL_VERSION)) {
                    return Err((-32000, "unsupported protocol version".into()));
                }
                Ok(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "pluginId": PLUGIN_ID,
                    "capabilities": ["transcription"],
                    "models": [{"id": MODEL_ID, "name": "Parakeet TDT 0.6B v3"}]
                }))
            }
            "transcription.loadModel" => {
                let started = Instant::now();
                let dir = params["path"].as_str().unwrap_or_default();
                let threads = params["threads"].as_i64().unwrap_or(4) as i32;
                self.recognizer = load(dir, threads);
                match self.recognizer {
                    Some(_) => Ok(json!({"loadMs": started.elapsed().as_millis() as u64})),
                    None => Err((-32001, "model could not be loaded".into())),
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
                let stream = recognizer.create_stream();
                stream.accept_waveform(16_000, &samples);
                recognizer.decode(&stream);
                let text = stream.get_result().map(|r| r.text).unwrap_or_default();
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

fn main() -> io::Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut plugin = Plugin {
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
        let reply = match plugin.handle(&method, message.get("params").unwrap_or(&json!({}))) {
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
