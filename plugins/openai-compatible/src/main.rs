//! OpenAI-compatible servers as a TypeWhisper plugin process: transcription
//! through `/v1/audio/transcriptions` or the `/v1/realtime` WebSocket, and text
//! through Chat Completions or the Responses API.
//!
//! The plugin keeps nothing on disk. The app sends the profiles and their keys
//! with `settings.update`; editor actions such as the connection test send the
//! unsaved profile with the request instead.

mod batch;
mod failure;
mod http;
mod llm;
mod models;
mod realtime;

use std::io::{self, Write};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use serde_json::{Value, json};
use tw_openai_compatible::{
    DEFAULT_PROFILE_ID, PLUGIN_ID, Profile, Profiles, SELECTED_PROFILE, secret_name,
};
use tw_plugin_protocol::{
    CAPABILITY_SETTINGS, Frame, PROTOCOL_VERSION, PluginSettings, SETTINGS_UPDATE, read_frame,
    write_frame,
};

use crate::batch::Options;
use crate::failure::Failure;

/// The plugin's setting with the profiles, under the macOS name.
pub const PROFILES: &str = "profiles";

/// A JSON-RPC `error` member.
type RpcResult = Result<Value, Value>;

/// Output shared with the realtime session thread, which sends notifications.
type Output = Arc<Mutex<Box<dyn Write + Send>>>;

/// The dictation between `transcription.begin` and `transcription.end`.
enum Pending {
    Batch {
        request_id: Value,
        profile: Box<Profile>,
        key: Option<String>,
        options: Options,
        samples: Vec<f32>,
    },
    Realtime {
        request_id: Value,
        session: realtime::Session,
        samples: usize,
    },
}

struct Plugin {
    output: Output,
    settings: PluginSettings,
    /// The profile dictations use, as `transcription.loadModel` chose it.
    selected: String,
    pending: Option<Pending>,
}

fn rpc_error(code: i64, message: &str) -> Value {
    json!({"code": code, "message": message})
}

fn options(params: &Value) -> Options {
    Options {
        language: params["language"].as_str().map(str::to_owned),
        prompt: params["prompt"].as_str().map(str::to_owned),
        translate: params["translate"].as_bool().unwrap_or(false),
    }
}

impl Plugin {
    fn profiles(&self) -> Profiles {
        Profiles::from_value(self.settings.values.get(PROFILES))
    }

    /// The profiles as the engine's models: the app's model picker chooses
    /// the profile, and `transcription.loadModel` gets its ID. All count as
    /// installed; one that lacks a URL or model fails to load with a reason.
    fn models(&self) -> Vec<Value> {
        self.profiles()
            .iter()
            .map(|profile| {
                json!({
                    "id": profile.id,
                    "name": profile.display_name(),
                    "recommended": profile.is_default(),
                    "sizeBytes": 0,
                    "installed": true
                })
            })
            .collect()
    }

    /// The profile and key a request uses: the editor's unsaved `profile` and
    /// `apiKey`, or the saved profile `profileId`, or the selected one. An
    /// editor without a new key uses the saved one, as on Windows.
    fn server(&self, params: &Value) -> Result<(Profile, Option<String>), Value> {
        let saved_key = |id: &str| self.settings.secret(&secret_name(id)).map(str::to_owned);
        if params["profile"].is_object() {
            let mut profile: Profile = serde_json::from_value(params["profile"].clone())
                .map_err(|_| rpc_error(-32602, "invalid profile"))?;
            profile.normalize();
            let key = params["apiKey"]
                .as_str()
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(str::to_owned)
                .or_else(|| saved_key(&profile.id));
            return Ok((profile, key));
        }
        let id = params["profileId"]
            .as_str()
            .unwrap_or(&self.selected)
            .to_owned();
        let profile = self
            .profiles()
            .get(&id)
            .cloned()
            .ok_or_else(|| Failure::NotConfigured(failure::NO_SERVER).to_error())?;
        let key = saved_key(&profile.id);
        Ok((profile, key))
    }

    fn handle(&mut self, method: &str, params: &Value) -> RpcResult {
        match method {
            "initialize" => {
                if params["protocolVersion"].as_u64() != Some(u64::from(PROTOCOL_VERSION)) {
                    return Err(rpc_error(-32000, "unsupported protocol version"));
                }
                Ok(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "pluginId": PLUGIN_ID,
                    "capabilities": ["transcription", "llm", CAPABILITY_SETTINGS],
                    "models": []
                }))
            }
            SETTINGS_UPDATE => {
                self.settings = serde_json::from_value(params.clone())
                    .map_err(|e| rpc_error(-32602, &format!("invalid settings: {e}")))?;
                // The profile the app saved as chosen, so that readiness is about
                // that one and not the default profile after a restart.
                let saved = self
                    .settings
                    .values
                    .get(SELECTED_PROFILE)
                    .and_then(Value::as_str);
                let profiles = self.profiles();
                if let Some(id) = saved.filter(|id| profiles.get(id).is_some()) {
                    self.selected = id.to_owned();
                } else if profiles.get(&self.selected).is_none() {
                    // The chosen profile was deleted: the default one takes
                    // over, which cannot be deleted.
                    let fallback = profiles
                        .get(DEFAULT_PROFILE_ID)
                        .or_else(|| profiles.iter().next());
                    if let Some(profile) = fallback {
                        self.selected = profile.id.clone();
                    }
                }
                let configured = self
                    .profiles()
                    .get(&self.selected)
                    .is_some_and(Profile::can_transcribe);
                Ok(json!({"configured": configured, "models": self.models()}))
            }
            // A server holds the models; there is nothing to load or download.
            "models.download" => Ok(json!({"installed": true})),
            "transcription.loadModel" => {
                if let Some(id) = params["modelId"].as_str() {
                    if self.profiles().get(id).is_none() {
                        return Err(rpc_error(-32001, &format!("unknown model {id}")));
                    }
                    self.selected = id.to_owned();
                }
                let (profile, _) = self.server(&json!({}))?;
                if profile.base_url.is_empty() {
                    return Err(Failure::NotConfigured(failure::NO_SERVER).to_error());
                }
                if profile.selected_model_id.is_empty() {
                    return Err(Failure::NotConfigured(failure::NO_TRANSCRIPTION_MODEL).to_error());
                }
                Ok(json!({"loadMs": 0}))
            }
            "transcription.begin" => {
                let (profile, key) = self.server(params)?;
                let options = options(params);
                let request_id = params["requestId"].clone();
                self.pending = None;
                self.pending = Some(if profile.uses_realtime() {
                    let session = realtime::Session::open(
                        &profile,
                        key.as_deref(),
                        &options,
                        partial_notifier(Arc::clone(&self.output), request_id.clone()),
                    )
                    .map_err(|f| f.to_error())?;
                    Pending::Realtime {
                        request_id,
                        session,
                        samples: 0,
                    }
                } else {
                    Pending::Batch {
                        request_id,
                        profile: Box::new(profile),
                        key,
                        options,
                        samples: Vec::new(),
                    }
                });
                Ok(json!({}))
            }
            "transcription.end" => {
                let pending = self
                    .pending
                    .take()
                    .ok_or_else(|| rpc_error(-32003, "no open request"))?;
                let started = Instant::now();
                let (request_id, samples, transcript) = match pending {
                    Pending::Batch {
                        request_id,
                        profile,
                        key,
                        options,
                        samples,
                    } => {
                        let transcript =
                            batch::transcribe(&profile, key.as_deref(), &samples, &options);
                        (request_id, samples.len(), transcript)
                    }
                    Pending::Realtime {
                        request_id,
                        session,
                        samples,
                    } => (request_id, samples, session.finish(samples)),
                };
                let mut result = transcript.map_err(|f| f.to_error())?.to_json();
                result["requestId"] = request_id;
                result["audioMs"] = json!(batch::audio_length(samples).as_millis() as u64);
                result["decodeMs"] = json!(started.elapsed().as_millis() as u64);
                Ok(result)
            }
            "connection.test" => {
                let (profile, key) = self.server(params)?;
                let catalog = models::fetch(&profile, key.as_deref()).map_err(|f| f.to_error())?;
                Ok(json!({"keyWithheld": catalog.key_withheld}))
            }
            "models.fetch" => {
                let (profile, key) = self.server(params)?;
                let catalog = models::fetch(&profile, key.as_deref()).map_err(|f| f.to_error())?;
                Ok(json!({"models": catalog.models, "keyWithheld": catalog.key_withheld}))
            }
            "llm.process" => {
                let (profile, key) = self.server(params)?;
                let text = llm::process(
                    &profile,
                    key.as_deref(),
                    params["systemPrompt"].as_str().unwrap_or_default(),
                    params["userText"].as_str().unwrap_or_default(),
                    params["model"].as_str(),
                )
                .map_err(|f| f.to_error())?;
                Ok(json!({"text": text}))
            }
            "shutdown" => Ok(json!({})),
            other => Err(rpc_error(-32601, &format!("unknown method {other}"))),
        }
    }

    fn audio(&mut self, samples: Vec<f32>) {
        match self.pending.as_mut() {
            Some(Pending::Batch { samples: all, .. }) => all.extend(samples),
            Some(Pending::Realtime {
                session,
                samples: count,
                ..
            }) => {
                *count += samples.len();
                session.send_audio(samples);
            }
            None => {}
        }
    }
}

/// Sends the realtime session's interim text as `transcription.partial`.
fn partial_notifier(output: Output, request_id: Value) -> Box<dyn FnMut(&str) + Send> {
    Box::new(move |text| {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "transcription.partial",
            "params": {"requestId": request_id, "text": text}
        });
        let mut output = output.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = write_frame(&mut *output, &Frame::Json(notification));
    })
}

fn main() -> io::Result<()> {
    let mut input = io::stdin().lock();
    let output: Output = Arc::new(Mutex::new(Box::new(io::stdout())));
    let mut plugin = Plugin {
        output: Arc::clone(&output),
        settings: PluginSettings::default(),
        selected: DEFAULT_PROFILE_ID.to_owned(),
        pending: None,
    };

    while let Some(frame) = read_frame(&mut input)? {
        let message = match frame {
            Frame::Pcm(samples) => {
                plugin.audio(samples);
                continue;
            }
            Frame::Json(message) => message,
        };
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message["method"].as_str().unwrap_or_default().to_owned();
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let reply = match plugin.handle(&method, &params) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        };
        let mut out = output.lock().unwrap_or_else(PoisonError::into_inner);
        write_frame(&mut *out, &Frame::Json(reply))?;
        drop(out);
        if method == "shutdown" {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin() -> Plugin {
        Plugin {
            output: Arc::new(Mutex::new(Box::new(io::sink()))),
            settings: PluginSettings::default(),
            selected: DEFAULT_PROFILE_ID.to_owned(),
            pending: None,
        }
    }

    fn configure(plugin: &mut Plugin) -> Value {
        let settings = json!({
            "values": {"profiles": [
                {"id": "openai-compatible", "name": "Local", "baseURL": "http://localhost:8000/v1", "selectedModelId": "whisper-1"},
                {"id": "openai-compatible-b", "name": "Cloud", "baseURL": "https://api.example.com"}
            ]},
            "secrets": {"api-key.openai-compatible-b": "saved-key"}
        });
        plugin.handle(SETTINGS_UPDATE, &settings).unwrap()
    }

    #[test]
    fn offers_the_profiles_as_models_and_loads_the_chosen_one() {
        let mut plugin = plugin();
        let configured = configure(&mut plugin);
        assert_eq!(configured["configured"], true);
        let models = configured["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "openai-compatible");
        assert_eq!(models[0]["recommended"], true);
        assert_eq!(models[1]["name"], "Cloud");
        assert!(models.iter().all(|m| m["installed"] == true));

        assert!(
            plugin
                .handle(
                    "transcription.loadModel",
                    &json!({"modelId": "openai-compatible"})
                )
                .is_ok()
        );
        // The cloud profile has no transcription model yet.
        let error = plugin
            .handle(
                "transcription.loadModel",
                &json!({"modelId": "openai-compatible-b"}),
            )
            .unwrap_err();
        assert_eq!(error["data"]["kind"], "notConfigured");
        assert_eq!(plugin.selected, "openai-compatible-b");
        let unknown = plugin.handle("transcription.loadModel", &json!({"modelId": "missing"}));
        assert!(unknown.is_err());
        assert_eq!(plugin.selected, "openai-compatible-b");
    }

    #[test]
    fn readiness_follows_the_saved_profile() {
        let mut plugin = plugin();
        let settings = |selected: &str| {
            json!({"values": {
                "profiles": [
                    {"id": "openai-compatible", "name": "Empty"},
                    {"id": "openai-compatible-b", "name": "Cloud", "baseURL": "https://api.example.com", "selectedModelId": "whisper-1"}
                ],
                "selectedProfileId": selected
            }})
        };
        let configured = plugin
            .handle(SETTINGS_UPDATE, &settings("openai-compatible-b"))
            .unwrap();
        assert_eq!(configured["configured"], true);
        assert_eq!(plugin.selected, "openai-compatible-b");
        // A saved ID without a profile leaves a choice that still exists alone.
        let configured = plugin.handle(SETTINGS_UPDATE, &settings("gone")).unwrap();
        assert_eq!(configured["configured"], true);
        assert_eq!(plugin.selected, "openai-compatible-b");

        // Deleting the chosen profile hands the choice to the default one.
        let only_default = json!({"values": {
            "profiles": [
                {"id": "openai-compatible", "name": "Local", "baseURL": "http://localhost:8000", "selectedModelId": "whisper-1"}
            ],
            "selectedProfileId": "openai-compatible-b"
        }});
        let configured = plugin.handle(SETTINGS_UPDATE, &only_default).unwrap();
        assert_eq!(configured["configured"], true);
        assert_eq!(plugin.selected, DEFAULT_PROFILE_ID);
    }

    #[test]
    fn uses_the_saved_or_the_editors_profile_and_key() {
        let mut plugin = plugin();
        configure(&mut plugin);
        let (profile, key) = plugin
            .server(&json!({"profileId": "openai-compatible-b"}))
            .unwrap();
        assert_eq!(
            (profile.name.as_str(), key.as_deref()),
            ("Cloud", Some("saved-key"))
        );
        let (profile, key) = plugin.server(&json!({})).unwrap();
        assert_eq!(
            (profile.base_url.as_str(), key),
            ("http://localhost:8000", None)
        );

        // The editor's unsaved values, without a new key: the saved one.
        let draft = json!({"profile": {"id": "openai-compatible-b", "name": "Renamed", "baseURL": "https://other.example.com/v1/"}});
        let (profile, key) = plugin.server(&draft).unwrap();
        assert_eq!(profile.base_url, "https://other.example.com");
        assert_eq!(key.as_deref(), Some("saved-key"));
        let typed = json!({"profile": {"id": "openai-compatible-b"}, "apiKey": " typed "});
        assert_eq!(plugin.server(&typed).unwrap().1.as_deref(), Some("typed"));
        let unknown = plugin.server(&json!({"profileId": "gone"})).unwrap_err();
        assert_eq!(unknown["data"]["kind"], "notConfigured");
    }

    #[test]
    fn reports_failed_requests_with_their_kind() {
        let mut plugin = plugin();
        configure(&mut plugin);
        let no_model = plugin
            .handle(
                "llm.process",
                &json!({"profileId": "openai-compatible", "userText": "x"}),
            )
            .unwrap_err();
        assert_eq!(no_model["code"], failure::REQUEST_FAILED);
        assert_eq!(no_model["data"]["kind"], "notConfigured");
        let realtime_translation = plugin.handle(
            "transcription.begin",
            &json!({"profile": {"id": "x", "baseURL": "http://localhost:1", "selectedModelId": "m", "transcriptionTransportRaw": "realtime"}, "translate": true}),
        );
        assert_eq!(
            realtime_translation.unwrap_err()["data"]["kind"],
            "invalidRequest"
        );
        let end = plugin.handle("transcription.end", &json!({})).unwrap_err();
        assert_eq!(end["code"], -32003);
    }
}
