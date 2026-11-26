//! Batch transcription: the whole recording as a WAV upload to
//! `/v1/audio/transcriptions`, with the WAV, form and answer of the shared
//! `tw-openai-transcription` crate.

use std::time::Duration;

use serde_json::{Value, json};
use tw_openai_compatible::endpoint;
use tw_openai_compatible::server_url::withholds_api_key;
use tw_openai_compatible::{BatchEndpoint, Profile};
use tw_openai_transcription as openai;

use crate::failure::{self, Failure};
use crate::http;

pub const SAMPLE_RATE: u32 = openai::SAMPLE_RATE;

/// Time for a request besides the length of its audio, so that the plugin gives
/// up before the host's watchdog does: the host allows 30 seconds plus the
/// length of the audio.
pub const REQUEST_TIME: Duration = Duration::from_secs(25);

/// No fixed upload limit: servers differ, and the RIFF header holds 4 GB.
const MAXIMUM_UPLOAD_BYTES: usize = u32::MAX as usize;

/// What the plugin needs besides the audio.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    /// An ISO language code, or `None` to let the server detect it.
    pub language: Option<String>,
    pub prompt: Option<String>,
    /// To English through `/audio/translations`.
    pub translate: bool,
}

/// The server's transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub text: String,
    pub language: Option<String>,
    pub duration_seconds: Option<f64>,
    pub no_speech_probability: Option<f64>,
}

impl Transcript {
    pub fn to_json(&self) -> Value {
        json!({
            "text": self.text,
            "language": self.language,
            "durationSeconds": self.duration_seconds,
            "noSpeechProbability": self.no_speech_probability,
        })
    }
}

/// The length of `samples` 16 kHz samples.
pub fn audio_length(samples: usize) -> Duration {
    Duration::from_millis(samples as u64 * 1000 / u64::from(SAMPLE_RATE))
}

pub fn transcribe(
    profile: &Profile,
    key: Option<&str>,
    samples: &[f32],
    options: &Options,
) -> Result<Transcript, Failure> {
    let model = profile.selected_model_id.trim();
    if model.is_empty() {
        return Err(Failure::NotConfigured(failure::NO_TRANSCRIPTION_MODEL));
    }
    let url = endpoint::batch_url(profile, options.translate)?;
    let wav = openai::encode_wav(samples, MAXIMUM_UPLOAD_BYTES)?;
    let withheld = withholds_api_key(&url, key);
    let timeout = REQUEST_TIME + audio_length(samples.len());
    let send = |format: &str| {
        let request = openai::Request {
            url: &url,
            // The key goes in the headers, under the key rule.
            api_key: None,
            model,
            language: options.language.as_deref(),
            prompt: options.prompt.as_deref(),
            response_format: format,
        };
        let body = openai::multipart(&request, &wav);
        http::post(&url, key, &body.content_type, &body.bytes, timeout)
    };
    let format = response_format(profile);
    let mut response = send(format)?;
    if format == "verbose_json" && refuses_verbose_json(response.status, &response.body) {
        // OpenAI's gpt-4o transcription models take only json and text.
        response = send("json")?;
    }
    let answer = http::success(response).map_err(|f| f.explain_withheld_key(withheld))?;
    parse_transcript(&answer)
}

/// A refusal that names the format, as OpenAI's for `gpt-4o-mini-transcribe`:
/// "response_format 'verbose_json' is not compatible with model …".
fn refuses_verbose_json(status: u16, body: &str) -> bool {
    status == 400 && body.contains("verbose_json")
}

/// Versioned and deployment-scoped servers (Azure) do not all offer
/// `verbose_json`, so they get `json`, as on Windows.
pub fn response_format(profile: &Profile) -> &'static str {
    if !profile.api_version.trim().is_empty()
        || profile.batch_endpoint() == BatchEndpoint::DeploymentScoped
    {
        "json"
    } else {
        "verbose_json"
    }
}

/// A language code without the `auto` sentinel.
pub fn language_hint(language: Option<&str>) -> Option<&str> {
    language
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("auto"))
}

/// Reads the answer; one without a `text` string is an error, as on Windows.
pub fn parse_transcript(body: &str) -> Result<Transcript, Failure> {
    let has_text = serde_json::from_str::<Value>(body).is_ok_and(|json| json["text"].is_string());
    if !has_text {
        return Err(Failure::InvalidResponse(
            "The provider returned no valid transcription text.".into(),
        ));
    }
    let transcript = openai::parse_response(body)?;
    Ok(Transcript {
        no_speech_probability: transcript.no_speech_probability(),
        text: transcript.text,
        language: transcript.language,
        duration_seconds: transcript.duration,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_versioned_and_deployment_scoped_servers_for_plain_json() {
        let mut profile = Profile::default();
        assert_eq!(response_format(&profile), "verbose_json");
        profile.api_version = "2025-03-01-preview".into();
        assert_eq!(response_format(&profile), "json");
        profile.api_version.clear();
        profile.batch_endpoint_raw = "deployment-scoped".into();
        assert_eq!(response_format(&profile), "json");
    }

    #[test]
    fn retries_with_json_only_when_the_server_names_verbose_json() {
        let openai = r#"{"error":{"message":"response_format 'verbose_json' is not compatible with model 'gpt-4o-mini-transcribe'. Use 'json' or 'text' instead."}}"#;
        assert!(refuses_verbose_json(400, openai));
        assert!(!refuses_verbose_json(400, r#"{"error":"unknown model"}"#));
        assert!(!refuses_verbose_json(500, openai));
    }

    #[test]
    fn drops_the_automatic_language_sentinel() {
        assert_eq!(language_hint(Some(" de ")), Some("de"));
        assert_eq!(language_hint(Some("AUTO")), None);
        assert_eq!(language_hint(Some("")), None);
        assert_eq!(language_hint(None), None);
    }

    #[test]
    fn reads_transcripts_and_rejects_invalid_ones() {
        let transcript = parse_transcript(
            r#"{"text":" hello ","language":"en","duration":"2.49","segments":[{"no_speech_prob":0.1}]}"#,
        )
        .unwrap();
        assert_eq!(transcript.text, "hello");
        assert_eq!(transcript.language.as_deref(), Some("en"));
        assert_eq!(transcript.duration_seconds, Some(2.49));
        assert_eq!(transcript.no_speech_probability, Some(0.1));
        let malformed = parse_transcript(r#"{"text":"ok","duration":{},"language":5}"#).unwrap();
        assert_eq!(
            (malformed.language, malformed.duration_seconds),
            (None, None)
        );
        for invalid in ["", "[]", r#"{"text":null}"#, r#"{"text":3}"#, "<html>"] {
            assert!(
                matches!(parse_transcript(invalid), Err(Failure::InvalidResponse(_))),
                "{invalid}"
            );
        }
    }
}
