//! Groq's hosted Whisper models as a TypeWhisper plugin process.
//!
//! The plugin keeps nothing on disk. The host gives it the API key and the
//! chosen model with `settings.update`, after `initialize` and whenever the user
//! changes them. Each dictation goes to Groq's OpenAI-style endpoint as a 16 kHz
//! WAV file, as in the Windows app.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tw_openai_transcription::{self as openai, ErrorKind};
use tw_plugin_protocol::{
    CAPABILITY_SETTINGS, ErrorData, Frame, PROTOCOL_VERSION, PluginSettings, SETTINGS_UPDATE,
    error_kind, read_frame, write_frame,
};

const PLUGIN_ID: &str = "com.typewhisper.groq";
const ENDPOINT: &str = "https://api.groq.com/openai/v1/audio/transcriptions";
/// The secret and the setting of the macOS plugin.
const API_KEY: &str = "api-key";
const SELECTED_MODEL: &str = "selectedModel";
/// Groq's limit for an uploaded file, about 13 minutes of audio.
const MAXIMUM_UPLOAD_BYTES: usize = 25_000_000;
/// Time for the request besides the length of the audio, so that the plugin
/// gives up before the host's watchdog does: the host allows 30 seconds plus
/// the length of the audio.
const REQUEST_TIME: Duration = Duration::from_secs(25);

struct Model {
    id: &'static str,
    name: &'static str,
}

/// The models of the macOS and Windows plugins, in their order. The first is
/// their default.
const MODELS: &[Model] = &[
    Model {
        id: "whisper-large-v3",
        name: "Whisper Large V3",
    },
    Model {
        id: "whisper-large-v3-turbo",
        name: "Whisper Large V3 Turbo",
    },
];

/// The Whisper language codes both models know, as the Windows plugin lists them.
const LANGUAGES: &str = "en zh de es ru ko fr ja pt tr pl ca nl ar sv it id hi fi vi he uk el ms cs \
    ro da hu ta no th ur hr bg lt la mi ml cy sk te fa lv bn sr az sl kn et mk br eu is hy ne mn bs \
    kk sq sw gl mr pa si km sn yo so af oc ka be tg sd gu am yi lo uz fo ht ps tk nn mt sa lb my bo \
    tl mg as tt haw ln ha ba jw su yue";

/// JSON-RPC error code of a failed transcription; `data.kind` says why.
const TRANSCRIPTION_FAILED: i64 = -32020;

type RpcError = (i64, String, Option<&'static str>);
type RpcResult = Result<Value, RpcError>;

struct Plugin {
    endpoint: String,
    api_key: Option<String>,
    model: &'static Model,
    /// The request ID, language and prompt of the open request, and its audio.
    pending: Option<Pending>,
}

struct Pending {
    request_id: Value,
    language: Option<String>,
    prompt: Option<String>,
    samples: Vec<f32>,
}

fn find_model(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.id == id)
}

impl Plugin {
    fn new(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.to_owned(),
            api_key: None,
            model: &MODELS[0],
            pending: None,
        }
    }

    fn handle(&mut self, method: &str, params: &Value) -> RpcResult {
        match method {
            "initialize" => {
                if params["protocolVersion"].as_u64() != Some(u64::from(PROTOCOL_VERSION)) {
                    return Err((-32000, "unsupported protocol version".into(), None));
                }
                let models: Vec<Value> = MODELS
                    .iter()
                    .enumerate()
                    .map(|(i, m)| {
                        json!({
                            "id": m.id, "name": m.name, "recommended": i == 0,
                            "languages": LANGUAGES.split_whitespace().collect::<Vec<_>>(),
                            // A cloud model needs no download.
                            "sizeBytes": 0, "installed": true
                        })
                    })
                    .collect();
                Ok(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "pluginId": PLUGIN_ID,
                    "capabilities": ["transcription", CAPABILITY_SETTINGS],
                    "models": models
                }))
            }
            SETTINGS_UPDATE => {
                let settings: PluginSettings = serde_json::from_value(params.clone())
                    .map_err(|e| (-32602, format!("invalid settings: {e}"), None))?;
                self.api_key = settings.secret(API_KEY).map(str::to_owned);
                let selected = settings.values.get(SELECTED_MODEL).and_then(Value::as_str);
                if let Some(model) = selected.and_then(find_model) {
                    self.model = model;
                }
                Ok(json!({"configured": self.api_key.is_some()}))
            }
            // Cloud models are always there.
            "models.download" => Ok(json!({"installed": true})),
            "transcription.loadModel" => {
                if let Some(id) = params["modelId"].as_str() {
                    self.model = find_model(id)
                        .ok_or_else(|| (-32001, format!("unknown model {id}"), None))?;
                }
                if self.api_key.is_none() {
                    return Err(not_configured());
                }
                Ok(json!({"loadMs": 0}))
            }
            "transcription.begin" => {
                let text = |name: &str| params[name].as_str().map(str::to_owned);
                self.pending = Some(Pending {
                    request_id: params["requestId"].clone(),
                    language: text("language"),
                    prompt: text("prompt"),
                    samples: Vec::new(),
                });
                Ok(json!({}))
            }
            "transcription.end" => {
                let pending =
                    self.pending
                        .take()
                        .ok_or((-32003, "no open request".to_string(), None))?;
                self.transcribe(pending)
            }
            "shutdown" => Ok(json!({})),
            other => Err((-32601, format!("unknown method {other}"), None)),
        }
    }

    fn transcribe(&self, pending: Pending) -> RpcResult {
        let api_key = self.api_key.as_deref().ok_or_else(not_configured)?;
        let started = Instant::now();
        let wav = openai::encode_wav(&pending.samples, MAXIMUM_UPLOAD_BYTES).map_err(|e| {
            if e.kind == ErrorKind::RequestTooLarge {
                // The text of the Windows app's Groq engine.
                let message =
                    "Recording exceeds Groq's 25 MB upload limit. Use a shorter recording.";
                (
                    TRANSCRIPTION_FAILED,
                    message.to_owned(),
                    Some(e.kind.name()),
                )
            } else {
                failure(e)
            }
        })?;
        let audio = Duration::from_millis(
            pending.samples.len() as u64 * 1000 / u64::from(openai::SAMPLE_RATE),
        );
        let request = openai::Request {
            url: &self.endpoint,
            api_key: Some(api_key),
            model: self.model.id,
            language: pending.language.as_deref(),
            prompt: pending.prompt.as_deref(),
            response_format: "verbose_json",
        };
        let mut transcript =
            openai::transcribe(&request, &wav, REQUEST_TIME + audio).map_err(failure)?;
        openai::drop_trailing_thank_you(&mut transcript);
        Ok(json!({
            "requestId": pending.request_id,
            "text": transcript.text,
            "language": transcript.language,
            "noSpeechProbability": transcript.no_speech_probability(),
            "audioMs": audio.as_millis() as u64,
            "decodeMs": started.elapsed().as_millis() as u64
        }))
    }
}

fn not_configured() -> RpcError {
    (
        TRANSCRIPTION_FAILED,
        "An API key is required for transcription.".into(),
        Some(error_kind::NOT_CONFIGURED),
    )
}

/// The service's error as a JSON-RPC error. The details go to the log; they
/// never hold the key.
fn failure(error: openai::Error) -> RpcError {
    eprintln!("Groq: {error}");
    (TRANSCRIPTION_FAILED, error.message, Some(error.kind.name()))
}

fn reply(id: Value, result: RpcResult) -> Value {
    match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message, kind)) => {
            let mut error = json!({"code": code, "message": message});
            if let Some(kind) = kind {
                error["data"] = json!(ErrorData { kind: kind.into() });
            }
            json!({"jsonrpc": "2.0", "id": id, "error": error})
        }
    }
}

fn serve(
    input: &mut impl io::Read,
    output: &mut impl Write,
    plugin: &mut Plugin,
) -> io::Result<()> {
    while let Some(frame) = read_frame(input)? {
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
        let result = plugin.handle(&method, &params);
        write_frame(output, &Frame::Json(reply(id, result)))?;
        if method == "shutdown" {
            break;
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    serve(
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        &mut Plugin::new(ENDPOINT),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;
    use std::sync::mpsc;

    fn initialize(plugin: &mut Plugin) -> Value {
        plugin
            .handle("initialize", &json!({"protocolVersion": PROTOCOL_VERSION}))
            .unwrap()
    }

    fn configure(plugin: &mut Plugin, settings: Value) -> Value {
        plugin.handle(SETTINGS_UPDATE, &settings).unwrap()
    }

    #[test]
    fn lists_the_models_and_takes_settings() {
        let mut plugin = Plugin::new(ENDPOINT);
        let info = initialize(&mut plugin);
        assert_eq!(info["pluginId"], PLUGIN_ID);
        assert_eq!(info["capabilities"], json!(["transcription", "settings"]));
        let ids: Vec<&str> = info["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["whisper-large-v3", "whisper-large-v3-turbo"]);
        assert_eq!(info["models"][0]["recommended"], true);
        assert_eq!(info["models"][1]["installed"], true);
        assert_eq!(
            info["models"][0]["languages"].as_array().unwrap().len(),
            100
        );

        let answer = configure(
            &mut plugin,
            json!({"values": {"selectedModel": "whisper-large-v3-turbo"}, "secrets": {"api-key": " gsk_x "}}),
        );
        assert_eq!(answer, json!({"configured": true}));
        assert_eq!(plugin.api_key.as_deref(), Some("gsk_x"));
        assert_eq!(plugin.model.id, "whisper-large-v3-turbo");
        // An unknown model keeps the current one; a later update without a key removes it.
        let answer = configure(&mut plugin, json!({"values": {"selectedModel": "nope"}}));
        assert_eq!(answer, json!({"configured": false}));
        assert_eq!(plugin.model.id, "whisper-large-v3-turbo");
        assert_eq!(plugin.api_key, None);
    }

    #[test]
    fn needs_a_key_before_it_transcribes() {
        let mut plugin = Plugin::new(ENDPOINT);
        initialize(&mut plugin);
        let error = plugin
            .handle(
                "transcription.loadModel",
                &json!({"modelId": "whisper-large-v3"}),
            )
            .unwrap_err();
        assert_eq!(error.2, Some(error_kind::NOT_CONFIGURED));
        let error = plugin
            .handle(
                "transcription.loadModel",
                &json!({"modelId": "whisper-large-v4"}),
            )
            .unwrap_err();
        assert_eq!(error.0, -32001);

        configure(&mut plugin, json!({"secrets": {"api-key": "gsk_x"}}));
        plugin
            .handle(
                "transcription.loadModel",
                &json!({"modelId": "whisper-large-v3-turbo"}),
            )
            .unwrap();
        assert_eq!(plugin.model.id, "whisper-large-v3-turbo");
    }

    #[test]
    fn refuses_recordings_over_the_upload_limit_before_sending() {
        // The endpoint does not exist, so a request would fail with a network error.
        let mut plugin = Plugin::new("http://127.0.0.1:9/never");
        initialize(&mut plugin);
        configure(&mut plugin, json!({"secrets": {"api-key": "gsk_x"}}));
        plugin
            .handle("transcription.begin", &json!({"requestId": "a"}))
            .unwrap();
        plugin.pending.as_mut().unwrap().samples = vec![0.0; (MAXIMUM_UPLOAD_BYTES - 44) / 2 + 1];
        let (code, message, kind) = plugin.handle("transcription.end", &json!({})).unwrap_err();
        assert_eq!(code, TRANSCRIPTION_FAILED);
        assert_eq!(kind, Some(error_kind::REQUEST_TOO_LARGE));
        assert_eq!(
            message,
            "Recording exceeds Groq's 25 MB upload limit. Use a shorter recording."
        );
    }

    /// A local stand-in for Groq that answers one request and reports what it got.
    fn fake_groq(
        status: &'static str,
        body: &'static str,
    ) -> (String, mpsc::Receiver<(String, Vec<u8>)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/openai/v1/audio/transcriptions",
            listener.local_addr().unwrap()
        );
        let (sent, received) = mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let length: usize = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap())
                })
                .unwrap();
            let mut content = vec![0; length];
            reader.read_exact(&mut content).unwrap();
            let answer = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            reader.get_mut().write_all(answer.as_bytes()).unwrap();
            sent.send((head, content)).unwrap();
        });
        (url, received)
    }

    /// Runs one dictation through the plugin's own loop, as the host would.
    fn dictate(plugin: &mut Plugin, language: Option<&str>, prompt: Option<&str>) -> Value {
        let mut input = Vec::new();
        let request = |id: u64, method: &str, params: Value| {
            Frame::Json(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
        };
        let mut begin = json!({"requestId": "dictation", "language": language});
        if let Some(prompt) = prompt {
            begin["prompt"] = json!(prompt);
        }
        for frame in [
            request(1, "transcription.begin", begin),
            Frame::Pcm(vec![0.25; 1600]),
            Frame::Pcm(vec![-0.25; 1600]),
            request(2, "transcription.end", json!({"requestId": "dictation"})),
        ] {
            write_frame(&mut input, &frame).unwrap();
        }
        let mut output = Vec::new();
        serve(&mut input.as_slice(), &mut output, plugin).unwrap();
        let mut reader = output.as_slice();
        read_frame(&mut reader).unwrap();
        match read_frame(&mut reader).unwrap() {
            Some(Frame::Json(reply)) => reply,
            other => panic!("no reply: {other:?}"),
        }
    }

    #[test]
    fn sends_the_dictation_to_groq() {
        let (url, received) = fake_groq(
            "200 OK",
            r#"{"text":" Hallo Welt. Thank you.","language":"German","duration":0.2,"segments":[
                {"text":" Hallo Welt.","start":0,"end":0.1,"no_speech_prob":0.01},
                {"text":" Thank you.","start":0.1,"end":0.2,"no_speech_prob":0.9}]}"#,
        );
        let mut plugin = Plugin::new(&url);
        initialize(&mut plugin);
        configure(
            &mut plugin,
            json!({"values": {"selectedModel": "whisper-large-v3-turbo"}, "secrets": {"api-key": "gsk_test"}}),
        );
        let reply = dictate(&mut plugin, Some("de"), Some("TypeWhisper"));
        assert_eq!(reply["result"]["text"], "Hallo Welt.");
        assert_eq!(reply["result"]["requestId"], "dictation");
        assert_eq!(reply["result"]["language"], "German");
        assert_eq!(reply["result"]["audioMs"], 200);

        let (head, content) = received.recv().unwrap();
        assert!(head.starts_with("POST /openai/v1/audio/transcriptions HTTP/1.1\r\n"));
        assert!(
            head.contains("\r\nauthorization: Bearer gsk_test\r\n")
                || head.contains("\r\nAuthorization: Bearer gsk_test\r\n")
        );
        let content = String::from_utf8_lossy(&content);
        for field in [
            "name=\"model\"\r\n\r\nwhisper-large-v3-turbo\r\n",
            "name=\"response_format\"\r\n\r\nverbose_json\r\n",
            "name=\"language\"\r\n\r\nde\r\n",
            "name=\"prompt\"\r\n\r\nTypeWhisper\r\n",
            "filename=\"audio.wav\"",
            "RIFF",
        ] {
            assert!(content.contains(field), "{field}");
        }
    }

    #[test]
    fn reports_a_rejected_key_by_its_kind() {
        let (url, received) = fake_groq(
            "401 Unauthorized",
            r#"{"error":{"message":"Invalid API Key","type":"invalid_request_error","code":"invalid_api_key"}}"#,
        );
        let mut plugin = Plugin::new(&url);
        initialize(&mut plugin);
        configure(&mut plugin, json!({"secrets": {"api-key": "gsk_wrong"}}));
        let reply = dictate(&mut plugin, None, None);
        assert_eq!(reply["error"]["code"], TRANSCRIPTION_FAILED);
        assert_eq!(reply["error"]["data"]["kind"], "authentication");
        assert_eq!(reply["error"]["message"], "Invalid API key");
        let (_, content) = received.recv().unwrap();
        let content = String::from_utf8_lossy(&content);
        assert!(!content.contains("name=\"language\"") && !content.contains("name=\"prompt\""));
        assert!(content.contains("whisper-large-v3\r\n"));
    }
}
