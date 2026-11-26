//! Request URLs and authentication headers for a profile.

use crate::profile::{BatchEndpoint, Profile, is_dated_api_version};
use crate::server_url::{self, UrlError};

/// Why a request cannot be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointError {
    /// The profile has no server URL.
    NoServer,
    Url(UrlError),
    DeploymentNeedsDatedVersion,
}

/// `base URL + path`, with `?api-version=` when the profile has one. The base
/// path stays, so `https://x.openai.azure.com/openai` gives `/openai/v1/models`.
pub fn request_url(profile: &Profile, path: &str) -> Result<String, EndpointError> {
    let base = server_url::validate(&profile.base_url).map_err(EndpointError::Url)?;
    if base.is_empty() {
        return Err(EndpointError::NoServer);
    }
    let mut url = format!("{base}/{}", path.trim_start_matches('/'));
    let version = profile.api_version.trim();
    if !version.is_empty() {
        url.push_str("?api-version=");
        url.push_str(&escape(version));
    }
    Ok(url)
}

pub fn models_url(profile: &Profile) -> Result<String, EndpointError> {
    request_url(profile, "/v1/models")
}

/// The batch upload URL: `/v1/audio/transcriptions`, or for deployment-scoped
/// servers such as Azure `/deployments/{model}/audio/transcriptions`, which
/// needs a dated API version. Translations use `translations` instead.
pub fn batch_url(profile: &Profile, translate: bool) -> Result<String, EndpointError> {
    let operation = if translate {
        "translations"
    } else {
        "transcriptions"
    };
    match profile.batch_endpoint() {
        BatchEndpoint::Standard => request_url(profile, &format!("/v1/audio/{operation}")),
        BatchEndpoint::DeploymentScoped => {
            if !is_dated_api_version(&profile.api_version) {
                return Err(EndpointError::DeploymentNeedsDatedVersion);
            }
            let model = escape(profile.selected_model_id.trim());
            request_url(profile, &format!("/deployments/{model}/audio/{operation}"))
        }
    }
}

/// The `/v1/realtime` WebSocket for transcription sessions.
pub fn realtime_url(profile: &Profile) -> Result<String, EndpointError> {
    let url = request_url(profile, "/v1/realtime")?;
    let url = match url.split_once("://") {
        Some(("https", rest)) => format!("wss://{rest}"),
        Some(("http", rest)) => format!("ws://{rest}"),
        _ => return Err(EndpointError::Url(UrlError::Scheme)),
    };
    let separator = if url.contains('?') { '&' } else { '?' };
    Ok(format!("{url}{separator}intent=transcription"))
}

pub fn chat_completions_url(profile: &Profile) -> Result<String, EndpointError> {
    request_url(profile, "/v1/chat/completions")
}

pub fn responses_url(profile: &Profile) -> Result<String, EndpointError> {
    request_url(profile, "/v1/responses")
}

/// Hosts of Azure OpenAI and Microsoft Foundry, which also want the key in an
/// `api-key` header.
const AZURE_HOST_SUFFIXES: &[&str] = &[
    ".openai.azure.com",
    ".openai.azure.us",
    ".services.ai.azure.com",
];

/// The headers that carry `key` to `url`: none without a key or where the key
/// rule withholds it, `Authorization: Bearer` otherwise, and `api-key` too for
/// Azure.
pub fn authentication_headers(url: &str, key: Option<&str>) -> Vec<(&'static str, String)> {
    let Some(key) = key.map(str::trim).filter(|k| !k.is_empty()) else {
        return Vec::new();
    };
    if !server_url::sends_api_key_to(url) {
        return Vec::new();
    }
    let mut headers = vec![("Authorization", format!("Bearer {key}"))];
    let azure = server_url::parse(url).is_ok_and(|parts| {
        let host = parts.host.to_ascii_lowercase();
        AZURE_HOST_SUFFIXES
            .iter()
            .any(|suffix| host.ends_with(suffix))
    });
    if azure {
        headers.push(("api-key", key.to_owned()));
    }
    headers
}

/// Percent-encodes everything but the unreserved characters, like .NET's
/// `Uri.EscapeDataString`.
fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            escaped.push(byte as char);
        } else {
            escaped.push_str(&format!("%{byte:02X}"));
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(url: &str) -> Profile {
        Profile {
            base_url: url.into(),
            ..Profile::default()
        }
    }

    #[test]
    fn keeps_the_base_path_and_adds_the_api_version() {
        let mut azure = profile("https://unit.openai.azure.com/openai");
        assert_eq!(
            models_url(&azure).unwrap(),
            "https://unit.openai.azure.com/openai/v1/models"
        );
        azure.api_version = "2025-03-01-preview".into();
        assert_eq!(
            models_url(&azure).unwrap(),
            "https://unit.openai.azure.com/openai/v1/models?api-version=2025-03-01-preview"
        );
        assert_eq!(
            chat_completions_url(&profile("http://localhost:1234/v1")).unwrap(),
            "http://localhost:1234/v1/chat/completions"
        );
        assert_eq!(
            responses_url(&profile("https://api.example.com")).unwrap(),
            "https://api.example.com/v1/responses"
        );
        assert_eq!(models_url(&profile("")), Err(EndpointError::NoServer));
        assert_eq!(
            models_url(&profile("https://a@b")),
            Err(EndpointError::Url(UrlError::Credentials))
        );
    }

    #[test]
    fn builds_standard_and_deployment_scoped_batch_urls() {
        let mut p = profile("https://api.groq.com/openai/v1");
        assert_eq!(
            batch_url(&p, false).unwrap(),
            "https://api.groq.com/openai/v1/audio/transcriptions"
        );
        assert_eq!(
            batch_url(&p, true).unwrap(),
            "https://api.groq.com/openai/v1/audio/translations"
        );
        p.batch_endpoint_raw = "deployment-scoped".into();
        p.selected_model_id = "my whisper/1".into();
        assert_eq!(
            batch_url(&p, false),
            Err(EndpointError::DeploymentNeedsDatedVersion)
        );
        p.api_version = "2025-03-01-preview".into();
        assert_eq!(
            batch_url(&p, false).unwrap(),
            "https://api.groq.com/openai/deployments/my%20whisper%2F1/audio/transcriptions?api-version=2025-03-01-preview"
        );
    }

    #[test]
    fn realtime_uses_websockets_and_the_transcription_intent() {
        assert_eq!(
            realtime_url(&profile("https://api.openai.com/v1")).unwrap(),
            "wss://api.openai.com/v1/realtime?intent=transcription"
        );
        let mut local = profile("http://localhost:8080");
        local.api_version = "preview".into();
        assert_eq!(
            realtime_url(&local).unwrap(),
            "ws://localhost:8080/v1/realtime?api-version=preview&intent=transcription"
        );
    }

    #[test]
    fn sends_the_key_where_the_rule_allows() {
        let headers = authentication_headers("https://api.example.com/v1/models", Some(" key "));
        assert_eq!(headers, [("Authorization", "Bearer key".to_owned())]);
        assert!(authentication_headers("http://192.168.1.5/v1/models", Some("key")).is_empty());
        assert!(authentication_headers("https://api.example.com/v1/models", Some(" ")).is_empty());
        assert!(authentication_headers("https://api.example.com/v1/models", None).is_empty());
        assert_eq!(
            authentication_headers("http://localhost/v1/models", Some("key")).len(),
            1
        );
    }

    #[test]
    fn azure_gets_the_api_key_header_under_the_same_rule() {
        for (url, azure) in [
            ("https://unit.openai.azure.com/openai/v1/models", true),
            ("https://unit.openai.azure.us/openai/v1/models", true),
            ("https://unit.services.ai.azure.com/openai/v1/models", true),
            (
                "https://openai.azure.com.attacker.invalid/openai/v1/models",
                false,
            ),
            ("http://localhost:1234/v1/models", false),
        ] {
            let headers = authentication_headers(url, Some("fixture"));
            assert_eq!(headers[0], ("Authorization", "Bearer fixture".to_owned()));
            assert_eq!(
                headers.iter().any(|(name, _)| *name == "api-key"),
                azure,
                "{url}"
            );
        }
        assert!(
            authentication_headers("http://demo.openai.azure.com/v1/models", Some("fixture"))
                .is_empty()
        );
    }
}
