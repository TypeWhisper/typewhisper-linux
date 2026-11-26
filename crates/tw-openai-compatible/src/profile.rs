//! Server profiles. Each one is a server with its own URL, key, models and
//! options, stored with the field names of the macOS app's profiles.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::server_url::{self, UrlError};

/// The profile that always exists and cannot be removed, as on macOS and Windows.
pub const DEFAULT_PROFILE_ID: &str = "openai-compatible";
pub const DEFAULT_PROFILE_NAME: &str = "OpenAI Compatible";
const PROFILE_ID_PREFIX: &str = "openai-compatible-";

/// Windows' text request timeout. macOS uses 30 seconds, which local models
/// often need more than.
pub const DEFAULT_TEXT_TIMEOUT_SECONDS: u32 = 300;
pub const MIN_TEXT_TIMEOUT_SECONDS: u32 = 5;
pub const MAX_TEXT_TIMEOUT_SECONDS: u32 = 3600;
pub const DEFAULT_TEMPERATURE: f64 = 0.3;
pub const MAX_TEMPERATURE: f64 = 2.0;

/// A model from the server's `/v1/models` list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FetchedModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<String>,
}

/// One server. Fields hold the macOS app's raw values; the accessors below
/// read them with the defaults of the other apps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Profile {
    pub id: String,
    pub name: String,
    #[serde(rename = "baseURL")]
    pub base_url: String,
    /// Sent as `?api-version=`; empty for most servers.
    pub api_version: String,
    /// The transcription model.
    pub selected_model_id: String,
    /// The text model.
    #[serde(rename = "selectedLLMModelId")]
    pub selected_llm_model_id: String,
    pub llm_temperature_mode_raw: String,
    pub llm_temperature_value: f64,
    pub fetched_models: Vec<FetchedModel>,
    pub chat_request_timeout_seconds: Option<f64>,
    pub thinking_enabled: bool,
    pub transcription_transport_raw: String,
    /// Not on macOS: the Windows app's choice of realtime protocol for
    /// deployment aliases.
    pub realtime_protocol_raw: String,
    pub batch_endpoint_raw: String,
    #[serde(rename = "llmAPIModeRaw")]
    pub llm_api_mode_raw: String,
    pub reasoning_effort_raw: String,
    /// Fields of other versions, such as the Mac's upload format, kept as they are.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            id: DEFAULT_PROFILE_ID.to_owned(),
            name: DEFAULT_PROFILE_NAME.to_owned(),
            base_url: String::new(),
            api_version: String::new(),
            selected_model_id: String::new(),
            selected_llm_model_id: String::new(),
            llm_temperature_mode_raw: TemperatureMode::ProviderDefault.as_str().to_owned(),
            llm_temperature_value: DEFAULT_TEMPERATURE,
            fetched_models: Vec::new(),
            chat_request_timeout_seconds: None,
            thinking_enabled: false,
            transcription_transport_raw: Transport::Auto.as_str().to_owned(),
            realtime_protocol_raw: RealtimeProtocol::Auto.as_str().to_owned(),
            batch_endpoint_raw: BatchEndpoint::Standard.as_str().to_owned(),
            llm_api_mode_raw: TextApi::ChatCompletions.as_str().to_owned(),
            reasoning_effort_raw: String::new(),
            other: Map::new(),
        }
    }
}

macro_rules! raw_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $raw:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &[$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $raw),+ }
            }

            /// The value for `raw`; unknown values give the first one.
            pub fn parse(raw: &str) -> Self {
                match raw { $($raw => $name::$variant,)+ _ => Self::ALL[0] }
            }
        }
    };
}

raw_enum!(
    /// How dictations reach the server.
    Transport { Auto = "auto", Batch = "batch", Realtime = "realtime" }
);
raw_enum!(
    /// Which realtime event format a model uses.
    RealtimeProtocol { Auto = "auto", Live = "live", Whisper = "whisper" }
);
raw_enum!(
    /// The path of batch transcription requests.
    BatchEndpoint { Standard = "standard", DeploymentScoped = "deployment-scoped" }
);
raw_enum!(
    /// The API that text requests use.
    TextApi { ChatCompletions = "chat-completions", Responses = "responses" }
);
raw_enum!(
    TemperatureMode { ProviderDefault = "providerDefault", Custom = "custom" }
);
raw_enum!(
    /// The Responses API's reasoning effort; `ProviderDefault` sends none.
    ReasoningEffort {
        ProviderDefault = "",
        Low = "low",
        Medium = "medium",
        High = "high",
        XHigh = "xhigh",
        Max = "max",
    }
);

/// Why a profile cannot be saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileError {
    Name,
    Url(UrlError),
    /// Deployment-scoped batch transcription needs a dated API version.
    DeploymentNeedsDatedVersion,
}

impl Profile {
    pub fn is_default(&self) -> bool {
        self.id == DEFAULT_PROFILE_ID
    }

    pub fn transport(&self) -> Transport {
        Transport::parse(&self.transcription_transport_raw)
    }

    pub fn realtime_protocol(&self) -> RealtimeProtocol {
        RealtimeProtocol::parse(&self.realtime_protocol_raw)
    }

    pub fn batch_endpoint(&self) -> BatchEndpoint {
        BatchEndpoint::parse(&self.batch_endpoint_raw)
    }

    pub fn text_api(&self) -> TextApi {
        TextApi::parse(&self.llm_api_mode_raw)
    }

    pub fn temperature_mode(&self) -> TemperatureMode {
        TemperatureMode::parse(&self.llm_temperature_mode_raw)
    }

    pub fn reasoning_effort(&self) -> ReasoningEffort {
        ReasoningEffort::parse(&self.reasoning_effort_raw)
    }

    /// The temperature to send, or `None` for the server's own. Responses
    /// requests with a reasoning effort send none, as reasoning models may
    /// reject it.
    pub fn temperature(&self) -> Option<f64> {
        let reasoning = self.text_api() == TextApi::Responses
            && self.reasoning_effort() != ReasoningEffort::ProviderDefault;
        (self.temperature_mode() == TemperatureMode::Custom && !reasoning)
            .then_some(self.llm_temperature_value)
    }

    pub fn text_timeout_seconds(&self) -> u32 {
        clamp_timeout(self.chat_request_timeout_seconds)
    }

    /// The name, or the default name when it is blank.
    pub fn display_name(&self) -> &str {
        match self.name.trim() {
            "" if self.is_default() => DEFAULT_PROFILE_NAME,
            "" => self.id.as_str(),
            name => name,
        }
    }

    /// True when dictations go over the `/v1/realtime` WebSocket. Auto picks it
    /// for the `gpt-live-transcribe` and `gpt-realtime-whisper` families,
    /// including dated snapshots; custom deployment aliases need Realtime.
    pub fn uses_realtime(&self) -> bool {
        match self.transport() {
            Transport::Realtime => true,
            Transport::Batch => false,
            Transport::Auto => {
                is_model_family(&self.selected_model_id, LIVE_MODEL_FAMILY)
                    || is_model_family(&self.selected_model_id, WHISPER_REALTIME_FAMILY)
            }
        }
    }

    /// True when the realtime session uses the Live Transcribe event format:
    /// always for an explicit Live, never for Whisper, and otherwise unless the
    /// model belongs to the `gpt-realtime-whisper` family.
    pub fn uses_live_protocol(&self) -> bool {
        match self.realtime_protocol() {
            RealtimeProtocol::Live => true,
            RealtimeProtocol::Whisper => false,
            RealtimeProtocol::Auto => {
                !is_model_family(&self.selected_model_id, WHISPER_REALTIME_FAMILY)
            }
        }
    }

    /// Ready for dictation: a server and a transcription model.
    pub fn can_transcribe(&self) -> bool {
        !self.base_url.is_empty() && !self.selected_model_id.trim().is_empty()
    }

    /// Brings stored values into range, as the Windows app does on start.
    pub fn normalize(&mut self) {
        self.id = self.id.trim().to_owned();
        self.name = self.name.trim().to_owned();
        self.base_url = server_url::normalize(&self.base_url);
        self.api_version = self.api_version.trim().to_owned();
        self.selected_model_id = self.selected_model_id.trim().to_owned();
        self.selected_llm_model_id = self.selected_llm_model_id.trim().to_owned();
        self.transcription_transport_raw = self.transport().as_str().to_owned();
        self.realtime_protocol_raw = self.realtime_protocol().as_str().to_owned();
        self.batch_endpoint_raw = self.batch_endpoint().as_str().to_owned();
        self.llm_api_mode_raw = self.text_api().as_str().to_owned();
        self.reasoning_effort_raw = self.reasoning_effort().as_str().to_owned();
        self.llm_temperature_mode_raw = self.temperature_mode().as_str().to_owned();
        self.llm_temperature_value = if self.llm_temperature_value.is_finite() {
            self.llm_temperature_value.clamp(0.0, MAX_TEMPERATURE)
        } else {
            DEFAULT_TEMPERATURE
        };
        if let Some(seconds) = self.chat_request_timeout_seconds {
            self.chat_request_timeout_seconds = Some(f64::from(clamp_timeout(Some(seconds))));
        }
        self.fetched_models = normalize_models(std::mem::take(&mut self.fetched_models));
    }

    /// Checks the fields a user edits and normalizes the URL.
    pub fn validate(&mut self) -> Result<(), ProfileError> {
        if self.name.trim().is_empty() {
            return Err(ProfileError::Name);
        }
        self.base_url = server_url::validate(&self.base_url).map_err(ProfileError::Url)?;
        if self.batch_endpoint() == BatchEndpoint::DeploymentScoped
            && !self.uses_realtime()
            && !is_dated_api_version(&self.api_version)
        {
            return Err(ProfileError::DeploymentNeedsDatedVersion);
        }
        self.normalize();
        Ok(())
    }

    /// The models to suggest, with the chosen ones even if the server did not
    /// list them, as manual model IDs stay possible.
    pub fn model_suggestions(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.fetched_models.iter().map(|m| m.id.clone()).collect();
        for chosen in [&self.selected_model_id, &self.selected_llm_model_id] {
            if !chosen.is_empty() && !ids.iter().any(|id| id == chosen) {
                ids.push(chosen.clone());
            }
        }
        ids
    }
}

/// The OpenAI realtime model families that Auto recognizes.
pub const LIVE_MODEL_FAMILY: &str = "gpt-live-transcribe";
pub const WHISPER_REALTIME_FAMILY: &str = "gpt-realtime-whisper";

/// True for `family` itself and its snapshots such as `family-2026-01-15`.
pub fn is_model_family(model: &str, family: &str) -> bool {
    let model = model.trim();
    model.eq_ignore_ascii_case(family)
        || model
            .get(..family.len() + 1)
            .is_some_and(|start| start.eq_ignore_ascii_case(&format!("{family}-")))
}

/// A version that starts with a date, such as `2025-03-01-preview`.
pub fn is_dated_api_version(version: &str) -> bool {
    let bytes = version.trim().as_bytes();
    bytes.len() >= 10
        && bytes[..10].iter().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                *b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
}

fn clamp_timeout(seconds: Option<f64>) -> u32 {
    match seconds {
        Some(s) if s.is_finite() => s.round().clamp(
            f64::from(MIN_TEXT_TIMEOUT_SECONDS),
            f64::from(MAX_TEXT_TIMEOUT_SECONDS),
        ) as u32,
        _ => DEFAULT_TEXT_TIMEOUT_SECONDS,
    }
}

/// Drops blank and repeated IDs (ignoring case) and sorts the rest.
pub fn normalize_models(models: Vec<FetchedModel>) -> Vec<FetchedModel> {
    let mut kept: Vec<FetchedModel> = Vec::new();
    for mut model in models {
        model.id = model.id.trim().to_owned();
        if !model.id.is_empty() && !kept.iter().any(|m| m.id.eq_ignore_ascii_case(&model.id)) {
            kept.push(model);
        }
    }
    kept.sort_by_key(|m| m.id.to_lowercase());
    kept
}

/// Name of the secret that holds a profile's key, as on macOS and Windows.
pub fn secret_name(profile_id: &str) -> String {
    if profile_id == DEFAULT_PROFILE_ID {
        "api-key".to_owned()
    } else {
        format!("api-key.{profile_id}")
    }
}

/// All profiles: the default one first, then the others by name.
#[derive(Debug, Clone, PartialEq)]
pub struct Profiles(Vec<Profile>);

impl Default for Profiles {
    fn default() -> Self {
        Self(vec![Profile::default()])
    }
}

impl Profiles {
    /// Reads a stored list. Entries that are not profiles are skipped, the
    /// default profile is added when it is missing, and repeated IDs keep the
    /// first entry.
    pub fn from_value(value: Option<&Value>) -> Self {
        let mut profiles: Vec<Profile> = Vec::new();
        for entry in value.and_then(Value::as_array).into_iter().flatten() {
            let Some(entry) = sanitize(entry) else {
                continue;
            };
            let Ok(mut profile) = serde_json::from_value::<Profile>(entry) else {
                continue;
            };
            profile.normalize();
            if profile.id.is_empty() {
                profile.id = new_profile_id();
            }
            if !profiles
                .iter()
                .any(|p| p.id.eq_ignore_ascii_case(&profile.id))
            {
                profiles.push(profile);
            }
        }
        let mut profiles = Self(profiles);
        profiles.sort();
        profiles
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(&self.0).unwrap_or(Value::Array(Vec::new()))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Profile> {
        self.0.iter()
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.0.iter().find(|p| p.id == id)
    }

    /// Adds an empty profile named `base_name`, or `base_name 2` and so on when
    /// that name is taken.
    pub fn add(&mut self, base_name: &str) -> Profile {
        let taken = |name: &str| self.0.iter().any(|p| p.name.eq_ignore_ascii_case(name));
        let mut name = base_name.to_owned();
        let mut suffix = 2;
        while taken(&name) {
            name = format!("{base_name} {suffix}");
            suffix += 1;
        }
        let mut id = new_profile_id();
        while self.get(&id).is_some() {
            id = new_profile_id();
        }
        let profile = Profile {
            id,
            name,
            ..Profile::default()
        };
        self.0.push(profile.clone());
        self.sort();
        profile
    }

    /// Adds `profile`, or replaces the one with its ID.
    pub fn insert(&mut self, profile: Profile) {
        match self.0.iter_mut().find(|p| p.id == profile.id) {
            Some(existing) => *existing = profile,
            None => self.0.push(profile),
        }
        self.sort();
    }

    /// Replaces the profile with the same ID. Returns false if there is none.
    pub fn update(&mut self, profile: Profile) -> bool {
        let Some(existing) = self.0.iter_mut().find(|p| p.id == profile.id) else {
            return false;
        };
        *existing = profile;
        self.sort();
        true
    }

    /// Removes a profile other than the default one.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.0.len();
        self.0.retain(|p| p.is_default() || p.id != id);
        self.0.len() != before
    }

    fn sort(&mut self) {
        if !self.0.iter().any(Profile::is_default) {
            self.0.insert(0, Profile::default());
        }
        self.0
            .sort_by_key(|p| (!p.is_default(), p.display_name().to_lowercase()));
    }
}

/// The stored entry without fields of the wrong type, which then take their
/// defaults instead of making the whole profile unreadable.
fn sanitize(entry: &Value) -> Option<Value> {
    let mut entry = entry.as_object()?.clone();
    entry.retain(|key, value| match key.as_str() {
        "id"
        | "name"
        | "baseURL"
        | "apiVersion"
        | "selectedModelId"
        | "selectedLLMModelId"
        | "llmTemperatureModeRaw"
        | "transcriptionTransportRaw"
        | "realtimeProtocolRaw"
        | "batchEndpointRaw"
        | "llmAPIModeRaw"
        | "reasoningEffortRaw" => value.is_string(),
        "llmTemperatureValue" => value.is_number(),
        "chatRequestTimeoutSeconds" => value.is_number() || value.is_null(),
        "thinkingEnabled" => value.is_boolean(),
        "fetchedModels" => value.is_array(),
        _ => true,
    });
    if let Some(Value::Array(models)) = entry.get_mut("fetchedModels") {
        models.retain(|model| model["id"].is_string());
        for model in models.iter_mut() {
            if let Some(model) = model.as_object_mut()
                && model
                    .get("owned_by")
                    .is_some_and(|owner| !owner.is_string())
            {
                model.remove("owned_by");
            }
        }
    }
    Some(Value::Object(entry))
}

/// `openai-compatible-` and 32 random hex digits, like a Windows profile ID.
fn new_profile_id() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    let read = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
    if read.is_err() {
        // A hash seeded at random by the standard library, mixed with the time.
        use std::hash::{BuildHasher, Hasher};
        for half in bytes.chunks_mut(8) {
            let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
            hasher.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default(),
            );
            half.copy_from_slice(&hasher.finish().to_le_bytes());
        }
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{PROFILE_ID_PREFIX}{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_and_writes_the_mac_field_names() {
        let stored = json!([{
            "id": "openai-compatible", "name": "Local", "baseURL": "http://localhost:8000/v1/",
            "apiVersion": "", "selectedModelId": "whisper-1", "selectedLLMModelId": "gemma",
            "llmTemperatureModeRaw": "custom", "llmTemperatureValue": 7.5,
            "fetchedModels": [{"id": "b"}, {"id": "a", "owned_by": "me"}, {"id": "A"}, {"id": " "}],
            "thinkingEnabled": true, "transcriptionTransportRaw": "batch",
            "batchEndpointRaw": "deployment-scoped", "uploadFormatRaw": "wav",
            "llmAPIModeRaw": "responses", "reasoningEffortRaw": "high", "serverKindRaw": "ollama"
        }]);
        let profiles = Profiles::from_value(Some(&stored));
        let profile = profiles.get(DEFAULT_PROFILE_ID).unwrap();
        assert_eq!(profile.base_url, "http://localhost:8000");
        assert_eq!(profile.llm_temperature_value, MAX_TEMPERATURE);
        assert_eq!(profile.transport(), Transport::Batch);
        assert_eq!(profile.batch_endpoint(), BatchEndpoint::DeploymentScoped);
        assert_eq!(profile.text_api(), TextApi::Responses);
        assert_eq!(profile.reasoning_effort(), ReasoningEffort::High);
        assert_eq!(profile.text_timeout_seconds(), DEFAULT_TEXT_TIMEOUT_SECONDS);
        let ids: Vec<&str> = profile
            .fetched_models
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, ["a", "b"]);

        let written = profiles.to_value();
        let entry = &written[0];
        assert_eq!(entry["baseURL"], "http://localhost:8000");
        assert_eq!(entry["selectedLLMModelId"], "gemma");
        assert_eq!(entry["llmAPIModeRaw"], "responses");
        // Fields only macOS knows survive a round trip.
        assert_eq!(entry["uploadFormatRaw"], "wav");
        assert_eq!(entry["serverKindRaw"], "ollama");
        assert_eq!(
            entry["fetchedModels"][0],
            json!({"id": "a", "owned_by": "me"})
        );
        assert_eq!(entry["fetchedModels"][1], json!({"id": "b"}));
    }

    #[test]
    fn unknown_values_fall_back_to_the_defaults() {
        let stored = json!([{
            "id": "x", "name": "X", "transcriptionTransportRaw": "carrier-pigeon",
            "llmAPIModeRaw": "?", "reasoningEffortRaw": "extreme", "llmTemperatureValue": null,
            "chatRequestTimeoutSeconds": 99999, "thinkingEnabled": "yes",
            "fetchedModels": [{"id": 7}, {"id": "ok", "owned_by": 3}]
        }, "not a profile", {"id": "X", "name": "duplicate"}]);
        let profiles = Profiles::from_value(Some(&stored));
        assert_eq!(
            profiles.iter().count(),
            2,
            "default profile added, duplicate dropped"
        );
        let profile = profiles.get("x").unwrap();
        assert_eq!(profile.transport(), Transport::Auto);
        assert_eq!(profile.text_api(), TextApi::ChatCompletions);
        assert_eq!(profile.reasoning_effort(), ReasoningEffort::ProviderDefault);
        assert_eq!(profile.llm_temperature_value, DEFAULT_TEMPERATURE);
        assert_eq!(profile.text_timeout_seconds(), MAX_TEXT_TIMEOUT_SECONDS);
        assert!(!profile.thinking_enabled);
        assert_eq!(
            profile.fetched_models,
            [FetchedModel {
                id: "ok".into(),
                owned_by: None
            }]
        );
        assert_eq!(Profiles::from_value(None), Profiles::default());
    }

    #[test]
    fn adds_distinct_names_and_keeps_the_default_profile() {
        let mut profiles = Profiles::default();
        let first = profiles.add("Custom Server");
        let second = profiles.add("Custom Server");
        assert_eq!(first.name, "Custom Server");
        assert_eq!(second.name, "Custom Server 2");
        assert_ne!(first.id, second.id);
        assert!(first.id.starts_with(PROFILE_ID_PREFIX));
        assert_eq!(first.id.len(), PROFILE_ID_PREFIX.len() + 32);

        assert!(!profiles.remove(DEFAULT_PROFILE_ID));
        assert!(profiles.remove(&first.id));
        assert!(!profiles.remove(&first.id));
        let names: Vec<&str> = profiles.iter().map(Profile::display_name).collect();
        assert_eq!(names, [DEFAULT_PROFILE_NAME, "Custom Server 2"]);

        let mut renamed = second.clone();
        renamed.name = "Aardvark".into();
        assert!(profiles.update(renamed));
        assert_eq!(profiles.iter().next().unwrap().id, DEFAULT_PROFILE_ID);
        assert!(!profiles.update(Profile {
            id: "missing".into(),
            ..Profile::default()
        }));
        let mut fresh = Profiles::default();
        let mut unsaved = fresh.clone().add("Custom Server");
        assert_eq!(
            fresh.iter().count(),
            1,
            "add on a copy leaves the list alone"
        );
        unsaved.name = "LM Studio".into();
        fresh.insert(unsaved.clone());
        fresh.insert(unsaved);
        assert_eq!(fresh.iter().count(), 2);
    }

    #[test]
    fn names_secrets_like_the_other_apps() {
        assert_eq!(secret_name(DEFAULT_PROFILE_ID), "api-key");
        assert_eq!(
            secret_name("openai-compatible-ab"),
            "api-key.openai-compatible-ab"
        );
    }

    #[test]
    fn auto_recognizes_the_realtime_model_families() {
        for (model, realtime, live) in [
            ("gpt-live-transcribe", true, true),
            ("GPT-Live-Transcribe-2026-01-15", true, true),
            ("gpt-realtime-whisper", true, false),
            ("gpt-realtime-whisper-2025-12-01", true, false),
            ("gpt-live-transcriber", false, true),
            ("whisper-1", false, true),
            ("", false, true),
        ] {
            let profile = Profile {
                selected_model_id: model.into(),
                ..Profile::default()
            };
            assert_eq!(profile.uses_realtime(), realtime, "{model}");
            assert_eq!(profile.uses_live_protocol(), live, "{model}");
        }
        let alias = Profile {
            selected_model_id: "my-deployment".into(),
            transcription_transport_raw: "realtime".into(),
            realtime_protocol_raw: "whisper".into(),
            ..Profile::default()
        };
        assert!(alias.uses_realtime());
        assert!(!alias.uses_live_protocol());
        let batch = Profile {
            selected_model_id: "gpt-live-transcribe".into(),
            transcription_transport_raw: "batch".into(),
            ..Profile::default()
        };
        assert!(!batch.uses_realtime());
    }

    #[test]
    fn reasoning_effort_leaves_out_the_temperature() {
        let mut profile = Profile {
            llm_temperature_mode_raw: "custom".into(),
            llm_temperature_value: 0.7,
            ..Profile::default()
        };
        assert_eq!(profile.temperature(), Some(0.7));
        profile.llm_api_mode_raw = "responses".into();
        assert_eq!(profile.temperature(), Some(0.7));
        profile.reasoning_effort_raw = "low".into();
        assert_eq!(profile.temperature(), None);
        profile.llm_temperature_mode_raw = "providerDefault".into();
        profile.reasoning_effort_raw = String::new();
        assert_eq!(profile.temperature(), None);
    }

    #[test]
    fn validates_what_the_editor_saves() {
        let mut profile = Profile {
            base_url: " https://example.com/openai/v1/ ".into(),
            ..Profile::default()
        };
        assert_eq!(profile.validate(), Ok(()));
        assert_eq!(profile.base_url, "https://example.com/openai");

        profile.name = "  ".into();
        assert_eq!(profile.validate(), Err(ProfileError::Name));
        profile.name = "Azure".into();
        profile.base_url = "https://user:pw@example.com".into();
        assert_eq!(
            profile.validate(),
            Err(ProfileError::Url(UrlError::Credentials))
        );
        profile.base_url = "https://unit.openai.azure.com/openai".into();
        profile.batch_endpoint_raw = "deployment-scoped".into();
        assert_eq!(
            profile.validate(),
            Err(ProfileError::DeploymentNeedsDatedVersion)
        );
        profile.api_version = "2025-03-01-preview".into();
        assert_eq!(profile.validate(), Ok(()));
        profile.api_version = "preview".into();
        profile.transcription_transport_raw = "realtime".into();
        assert_eq!(
            profile.validate(),
            Ok(()),
            "realtime ignores the batch endpoint"
        );
    }

    #[test]
    fn suggests_fetched_and_manual_models() {
        let profile = Profile {
            fetched_models: vec![FetchedModel {
                id: "whisper-1".into(),
                owned_by: None,
            }],
            selected_model_id: "whisper-1".into(),
            selected_llm_model_id: "my-model".into(),
            ..Profile::default()
        };
        assert_eq!(profile.model_suggestions(), ["whisper-1", "my-model"]);
    }

    #[test]
    fn recognizes_dated_api_versions() {
        assert!(is_dated_api_version("2025-03-01-preview"));
        assert!(is_dated_api_version(" 2024-10-21 "));
        assert!(!is_dated_api_version("preview"));
        assert!(!is_dated_api_version("2025-3-01"));
        assert!(!is_dated_api_version(""));
    }
}
