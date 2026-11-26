//! Speech to text over an OpenAI-style `/audio/transcriptions` endpoint, as
//! Groq, OpenAI and many self-hosted servers offer it. Cloud plugins share it:
//! the WAV encoding, the multipart upload, the HTTP errors and the answer.
//!
//! Ported from the Windows app's plugin SDK (`PcmWaveEncoder`,
//! `OpenAiTranscriptionHelper` and `OpenAiApiHelper`), so that the requests and
//! the error handling match.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

use serde_json::Value;
use tw_plugin_protocol::error_kind;

/// Sample rate of the audio the plugin protocol carries.
pub const SAMPLE_RATE: u32 = 16_000;
const WAV_HEADER_BYTES: usize = 44;

/// What went wrong, in the terms of the plugin protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Authentication,
    Permission,
    RateLimit,
    Network,
    Timeout,
    RequestTooLarge,
    InvalidRequest,
    Server,
}

impl ErrorKind {
    /// The name the plugin protocol uses, see [`tw_plugin_protocol::error_kind`].
    pub fn name(self) -> &'static str {
        match self {
            ErrorKind::Authentication => error_kind::AUTHENTICATION,
            ErrorKind::Permission => error_kind::PERMISSION,
            ErrorKind::RateLimit => error_kind::RATE_LIMIT,
            ErrorKind::Network => error_kind::NETWORK,
            ErrorKind::Timeout => error_kind::TIMEOUT,
            ErrorKind::RequestTooLarge => error_kind::REQUEST_TOO_LARGE,
            ErrorKind::InvalidRequest => error_kind::INVALID_REQUEST,
            ErrorKind::Server => error_kind::SERVER,
        }
    }
}

/// A failed transcription. `message` is English and never holds the API key.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    /// The HTTP status, when the service answered.
    pub status: Option<u16>,
}

impl Error {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// The size of the WAV file that [`encode_wav`] makes of `samples` samples.
pub fn wav_bytes(samples: usize) -> usize {
    WAV_HEADER_BYTES + 2 * samples
}

/// Encodes 16 kHz mono samples as 16-bit PCM WAV, as the Windows app does: no
/// codec, and small enough for the services' upload limits. Checks the size
/// before it allocates the file.
pub fn encode_wav(samples: &[f32], maximum_bytes: usize) -> Result<Vec<u8>, Error> {
    if samples.is_empty() {
        return Err(Error::new(ErrorKind::InvalidRequest, "No audio captured."));
    }
    let size = wav_bytes(samples.len());
    // The RIFF header counts in 32 bits.
    if size > maximum_bytes || u32::try_from(size).is_err() {
        return Err(Error::new(
            ErrorKind::RequestTooLarge,
            "Recording exceeds the selected provider's upload limit. Use a shorter recording.",
        ));
    }
    let data = (2 * samples.len()) as u32;
    let mut wav = Vec::with_capacity(size);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(2 * SAMPLE_RATE).to_le_bytes()); // bytes per second
    wav.extend_from_slice(&2u16.to_le_bytes()); // bytes per frame
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data.to_le_bytes());
    for &sample in samples {
        if !sample.is_finite() {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "Audio contains invalid samples.",
            ));
        }
        // .NET's Math.Round rounds halves to even.
        let scaled = (f64::from(sample.clamp(-1.0, 1.0)) * 32768.0).round_ties_even();
        wav.extend_from_slice(&(scaled.clamp(-32768.0, 32767.0) as i16).to_le_bytes());
    }
    Ok(wav)
}

/// One transcription request.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    /// The full endpoint, for example `https://api.groq.com/openai/v1/audio/transcriptions`.
    pub url: &'a str,
    /// Sent as a bearer token. `None` for servers that take anonymous requests.
    pub api_key: Option<&'a str>,
    pub model: &'a str,
    /// A language code such as `de`. `None`, empty and `auto` leave the
    /// language to the service.
    pub language: Option<&'a str>,
    /// Words that help the model spell names and terms. Blank ones are left out.
    pub prompt: Option<&'a str>,
    /// `verbose_json` gives segments with their no-speech probability.
    pub response_format: &'a str,
}

/// A request body with the type that names its boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct Body {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// The multipart form OpenAI's API reads: the audio first, then the model,
/// the format and the optional language and prompt, as the Windows app sends them.
pub fn multipart(request: &Request<'_>, wav: &[u8]) -> Body {
    let mut fields = vec![
        ("model", request.model),
        ("response_format", request.response_format),
    ];
    if let Some(language) = request
        .language
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "auto")
    {
        fields.push(("language", language));
    }
    if let Some(prompt) = request.prompt.filter(|p| !p.trim().is_empty()) {
        fields.push(("prompt", prompt));
    }
    let boundary = boundary(|candidate| {
        let needle = candidate.as_bytes();
        let contains = |haystack: &[u8]| haystack.windows(needle.len()).any(|w| w == needle);
        contains(wav) || fields.iter().any(|(_, value)| contains(value.as_bytes()))
    });

    let mut bytes = Vec::with_capacity(wav.len() + 512);
    bytes.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    bytes.extend_from_slice(wav);
    for (name, value) in &fields {
        bytes.extend_from_slice(
            format!(
                "\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}"
            )
            .as_bytes(),
        );
    }
    bytes.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Body {
        content_type: format!("multipart/form-data; boundary={boundary}"),
        bytes,
    }
}

/// A random boundary that `taken` says occurs nowhere in the content.
fn boundary(mut taken: impl FnMut(&str) -> bool) -> String {
    loop {
        // Each RandomState has its own random keys, so this needs no RNG crate.
        let random = |_| RandomState::new().build_hasher().finish();
        let candidate = format!("TypeWhisper{:016x}{:016x}", random(0), random(1));
        if !taken(&candidate) {
            return candidate;
        }
    }
}

/// Sends the request and reads the answer. `timeout` covers the whole
/// request, from connecting to the last byte of the answer.
pub fn transcribe(
    request: &Request<'_>,
    wav: &[u8],
    timeout: Duration,
) -> Result<Transcript, Error> {
    let body = multipart(request, wav);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into();
    let mut post = agent
        .post(request.url)
        .header("Content-Type", &body.content_type);
    if let Some(key) = request.api_key {
        post = post.header("Authorization", format!("Bearer {key}"));
    }
    let mut response = post.send(&body.bytes[..]).map_err(transport_error)?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(transport_error)?;
    if !(200..300).contains(&status) {
        return Err(status_error(status, &text));
    }
    parse_response(&text)
}

fn transport_error(error: ureq::Error) -> Error {
    match error {
        ureq::Error::Timeout(_) => Error::new(ErrorKind::Timeout, "API request timed out."),
        other => Error::new(ErrorKind::Network, format!("Network error: {other}")),
    }
}

/// The error for an answer with an HTTP error status, as the Windows app's
/// `OpenAiApiHelper` classifies them.
pub fn status_error(status: u16, body: &str) -> Error {
    let kind = match status {
        401 => ErrorKind::Authentication,
        403 => ErrorKind::Permission,
        408 => ErrorKind::Timeout,
        413 => ErrorKind::RequestTooLarge,
        429 => ErrorKind::RateLimit,
        500..=599 => ErrorKind::Server,
        400..=499 => ErrorKind::InvalidRequest,
        _ => ErrorKind::Server,
    };
    let message = match status {
        401 => "Invalid API key".to_owned(),
        413 => "Audio too large".to_owned(),
        429 => "Rate limit reached, please wait".to_owned(),
        _ => format!("API error {status}: {}", error_message(body)),
    };
    Error {
        kind,
        message,
        status: Some(status),
    }
}

/// The message of an OpenAI-style error body, or the start of the body.
pub fn error_message(body: &str) -> String {
    if let Ok(json) = serde_json::from_str::<Value>(body) {
        let error = &json["error"];
        if let Some(message) = error["message"].as_str().or_else(|| error.as_str()) {
            return message.to_owned();
        }
    }
    body.chars().take(200).collect()
}

/// What the service heard.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript {
    pub text: String,
    /// The language the service detected, when it says.
    pub language: Option<String>,
    /// Length of the audio in seconds, when the service says.
    pub duration: Option<f64>,
    pub segments: Vec<Segment>,
}

impl Transcript {
    /// The lowest no-speech probability of any segment: high values mean that
    /// the whole recording was probably silence.
    pub fn no_speech_probability(&self) -> Option<f64> {
        self.segments
            .iter()
            .filter_map(|s| s.no_speech_probability)
            .reduce(f64::min)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Segment {
    pub text: String,
    pub start: f64,
    pub end: f64,
    pub no_speech_probability: Option<f64>,
}

/// Reads a `json` or `verbose_json` answer. Some servers send the numbers as
/// strings, such as `"2.49"`; those count too.
pub fn parse_response(body: &str) -> Result<Transcript, Error> {
    let json: Value = serde_json::from_str(body).map_err(|_| {
        Error::new(
            ErrorKind::Server,
            "The service answered with something other than JSON.",
        )
    })?;
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    let segments = json["segments"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|segment| Segment {
            text: text(&segment["text"]),
            start: number(&segment["start"]).unwrap_or(0.0),
            end: number(&segment["end"]).unwrap_or(0.0),
            no_speech_probability: number(&segment["no_speech_prob"]),
        })
        .collect();
    Ok(Transcript {
        text: text(&json["text"]).trim().to_owned(),
        language: json["language"].as_str().map(str::to_owned),
        duration: number(&json["duration"]),
        segments,
    })
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Above this no-speech probability, a closing "Thank you" is taken for one of
/// Whisper's hallucinations on silence.
const THANK_YOU_NO_SPEECH: f64 = 0.8;

/// Drops a final "Thank you." that Whisper made up for the silence at the end,
/// as the Windows app's Groq plugin does: only when the last segment is nothing
/// but those words, is most likely silence, and other text comes before it.
pub fn drop_trailing_thank_you(transcript: &mut Transcript) {
    let Some(last) = transcript.segments.last() else {
        return;
    };
    let said = last.text.trim();
    if !last
        .no_speech_probability
        .is_some_and(|p| p > THANK_YOU_NO_SPEECH)
        || !is_thank_you(said)
        || said.is_empty()
    {
        return;
    }
    let Some(before) = transcript.text.trim().strip_suffix(said) else {
        return;
    };
    let before = before.trim_end();
    if before.is_empty() {
        return;
    }
    transcript.text = before.to_owned();
    transcript.segments.pop();
}

/// "Thank you" with any case, spacing and punctuation.
fn is_thank_you(text: &str) -> bool {
    let letters: String = text
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    letters == "thankyou"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encodes_a_wav_header_and_pcm16_samples() {
        let wav = encode_wav(
            &[0.0, 0.5, -0.5, 1.0, -1.0, 2.0, 0.25 / 32768.0],
            usize::MAX,
        )
        .unwrap();
        assert_eq!(wav.len(), wav_bytes(7));
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 36 + 14);
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(wav[28..32].try_into().unwrap()), 32_000);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 14);
        let samples: Vec<i16> = wav[44..]
            .chunks(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        // Full scale clips to the 16-bit range; values beyond it are clamped first.
        assert_eq!(samples, [0, 16384, -16384, 32767, -32768, 32767, 0]);
    }

    #[test]
    fn refuses_empty_invalid_and_oversized_audio() {
        assert_eq!(
            encode_wav(&[], usize::MAX).unwrap_err().kind,
            ErrorKind::InvalidRequest
        );
        assert_eq!(
            encode_wav(&[f32::NAN], usize::MAX).unwrap_err().kind,
            ErrorKind::InvalidRequest
        );
        // 25 MB holds 12,499,978 samples, about 13 minutes.
        let limit = 25_000_000;
        let fits = (limit - 44) / 2;
        assert_eq!(wav_bytes(fits), limit);
        let too_large = encode_wav(&vec![0.0; fits + 1], limit).unwrap_err();
        assert_eq!(too_large.kind, ErrorKind::RequestTooLarge);
        assert_eq!(
            too_large.message,
            "Recording exceeds the selected provider's upload limit. Use a shorter recording."
        );
        assert_eq!(encode_wav(&vec![0.0; fits], limit).unwrap().len(), limit);
    }

    fn request<'a>(language: Option<&'a str>, prompt: Option<&'a str>) -> Request<'a> {
        Request {
            url: "https://api.groq.com/openai/v1/audio/transcriptions",
            api_key: Some("gsk_test"),
            model: "whisper-large-v3-turbo",
            language,
            prompt,
            response_format: "verbose_json",
        }
    }

    /// The parts of a multipart body as (headers, content).
    fn parts(body: &Body) -> Vec<(String, Vec<u8>)> {
        let boundary = body.content_type.split("boundary=").nth(1).unwrap();
        let delimiter = format!("--{boundary}");
        let text = &body.bytes;
        let mut found = Vec::new();
        let positions: Vec<usize> = (0..text.len())
            .filter(|&i| text[i..].starts_with(delimiter.as_bytes()))
            .collect();
        for pair in positions.windows(2) {
            let part = &text[pair[0] + delimiter.len() + 2..pair[1] - 2];
            let split = part.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            found.push((
                String::from_utf8(part[..split].to_vec()).unwrap(),
                part[split + 4..].to_vec(),
            ));
        }
        assert!(text.ends_with(format!("{delimiter}--\r\n").as_bytes()));
        found
    }

    #[test]
    fn builds_the_form_with_language_and_prompt() {
        let wav = encode_wav(&[0.1; 160], usize::MAX).unwrap();
        let body = multipart(&request(Some("de"), Some("TypeWhisper, Groq")), &wav);
        assert!(
            body.content_type
                .starts_with("multipart/form-data; boundary=TypeWhisper")
        );
        let parts = parts(&body);
        assert_eq!(parts.len(), 5);
        assert!(parts[0].0.contains("name=\"file\"; filename=\"audio.wav\""));
        assert!(parts[0].0.contains("Content-Type: audio/wav"));
        assert_eq!(parts[0].1, wav);
        let fields: Vec<(String, String)> = parts[1..]
            .iter()
            .map(|(headers, value)| {
                let name = headers
                    .split("name=\"")
                    .nth(1)
                    .unwrap()
                    .trim_end_matches('"');
                (name.to_owned(), String::from_utf8(value.clone()).unwrap())
            })
            .collect();
        assert_eq!(
            fields,
            [
                ("model".into(), "whisper-large-v3-turbo".into()),
                ("response_format".into(), "verbose_json".into()),
                ("language".into(), "de".into()),
                ("prompt".into(), "TypeWhisper, Groq".into()),
            ]
        );
        // The key goes in a header, never into the body.
        assert!(!String::from_utf8_lossy(&body.bytes).contains("gsk_test"));
    }

    #[test]
    fn leaves_out_automatic_language_and_blank_prompt() {
        let wav = encode_wav(&[0.0; 16], usize::MAX).unwrap();
        for (language, prompt) in [
            (None, None),
            (Some("auto"), Some("  ")),
            (Some(""), Some("")),
        ] {
            let parts = parts(&multipart(&request(language, prompt), &wav));
            assert_eq!(parts.len(), 3, "{language:?} {prompt:?}");
            let text = String::from_utf8_lossy(&multipart(&request(language, prompt), &wav).bytes)
                .into_owned();
            assert!(!text.contains("name=\"language\"") && !text.contains("name=\"prompt\""));
        }
    }

    #[test]
    fn picks_a_boundary_the_content_does_not_contain() {
        let mut tries = 0;
        let picked = boundary(|_| {
            tries += 1;
            tries < 3
        });
        assert_eq!(tries, 3);
        assert!(picked.starts_with("TypeWhisper") && picked.len() == 43);
    }

    #[test]
    fn classifies_http_errors_like_the_windows_app() {
        let groq = r#"{"error":{"message":"The model `whisper-large-v4` does not exist","type":"invalid_request_error"}}"#;
        let cases = [
            (401, ErrorKind::Authentication, "Invalid API key"),
            (
                403,
                ErrorKind::Permission,
                "API error 403: The model `whisper-large-v4` does not exist",
            ),
            (
                408,
                ErrorKind::Timeout,
                "API error 408: The model `whisper-large-v4` does not exist",
            ),
            (413, ErrorKind::RequestTooLarge, "Audio too large"),
            (429, ErrorKind::RateLimit, "Rate limit reached, please wait"),
            (
                400,
                ErrorKind::InvalidRequest,
                "API error 400: The model `whisper-large-v4` does not exist",
            ),
            (
                503,
                ErrorKind::Server,
                "API error 503: The model `whisper-large-v4` does not exist",
            ),
        ];
        for (status, kind, message) in cases {
            let error = status_error(status, groq);
            assert_eq!(
                (error.kind, error.message.as_str(), error.status),
                (kind, message, Some(status))
            );
        }
        assert_eq!(error_message(r#"{"error": "plain"}"#), "plain");
        assert_eq!(error_message(&"x".repeat(300)).len(), 200);
        assert_eq!(ErrorKind::RateLimit.name(), "rateLimit");
    }

    #[test]
    fn reads_a_verbose_answer() {
        let body = json!({
            "task": "transcribe", "language": "German", "duration": 2.49,
            "text": " Hallo Welt. ",
            "segments": [
                {"id": 0, "start": 0.0, "end": 1.2, "text": " Hallo", "no_speech_prob": 0.02},
                {"id": 1, "start": "1.2", "end": "2.49", "text": " Welt.", "no_speech_prob": "0.1"}
            ]
        });
        let transcript = parse_response(&body.to_string()).unwrap();
        assert_eq!(transcript.text, "Hallo Welt.");
        assert_eq!(transcript.language.as_deref(), Some("German"));
        assert_eq!(transcript.duration, Some(2.49));
        assert_eq!(transcript.segments[1].start, 1.2);
        assert_eq!(transcript.segments[1].end, 2.49);
        assert_eq!(transcript.no_speech_probability(), Some(0.02));

        let plain = parse_response(r#"{"text": "Hi"}"#).unwrap();
        assert_eq!((plain.text.as_str(), plain.segments.len()), ("Hi", 0));
        assert_eq!(plain.no_speech_probability(), None);
        assert_eq!(parse_response("Hi").unwrap_err().kind, ErrorKind::Server);
    }

    fn with_ending(text: &str, last: &str, no_speech: f64) -> Transcript {
        Transcript {
            text: text.into(),
            segments: vec![
                Segment {
                    text: " Send the report.".into(),
                    no_speech_probability: Some(0.01),
                    ..Segment::default()
                },
                Segment {
                    text: last.into(),
                    no_speech_probability: Some(no_speech),
                    ..Segment::default()
                },
            ],
            ..Transcript::default()
        }
    }

    #[test]
    fn drops_a_made_up_thank_you_at_the_end() {
        let mut made_up = with_ending("Send the report. Thank you.", " Thank you.", 0.95);
        drop_trailing_thank_you(&mut made_up);
        assert_eq!(made_up.text, "Send the report.");
        assert_eq!(made_up.segments.len(), 1);

        // Spoken thanks, other words, or thanks alone stay.
        for (text, last, no_speech) in [
            ("Send the report. Thank you.", " Thank you.", 0.3),
            ("Send the report. Thanks.", " Thanks.", 0.95),
            ("Thank you.", " Thank you.", 0.95),
        ] {
            let mut kept = with_ending(text, last, no_speech);
            drop_trailing_thank_you(&mut kept);
            assert_eq!(kept.text, text);
        }
        assert!(is_thank_you("THANK-YOU!"));
    }
}
