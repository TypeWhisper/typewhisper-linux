//! Why a request failed, in the terms of the plugin protocol: a kind from
//! [`error_kind`] the app explains in its own words, an English message, and
//! the server's own text where it gave one.

use serde_json::{Value, json};
use tw_openai_compatible::endpoint::EndpointError;
use tw_openai_transcription as openai;
use tw_plugin_protocol::{ErrorData, error_kind};

/// JSON-RPC error code of a failed request; `data.kind` says why.
pub const REQUEST_FAILED: i64 = -32020;

#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// The server answered with an error status, or could not be reached.
    Service {
        error: openai::Error,
        /// The message in the server's answer.
        detail: Option<String>,
    },
    /// The profile lacks something a request needs; the English text says what.
    NotConfigured(&'static str),
    /// The server asked for the key, but the key rule kept it from this server.
    KeyWithheld,
    /// The answer was not what the API describes.
    InvalidResponse(String),
    /// A language model stopped at its token limit.
    Truncated,
    /// The language model returned no text.
    EmptyText,
    /// Realtime sessions cannot translate.
    RealtimeTranslation,
    /// The realtime server refused the session, with its reason.
    RealtimeRejected(String),
    /// The realtime connection broke or a step took too long.
    Realtime(&'static str),
}

pub const NO_SERVER: &str = "Server URL not configured";
pub const INVALID_URL: &str =
    "Enter an HTTP or HTTPS server URL without credentials, a query, or a fragment.";
pub const NO_TRANSCRIPTION_MODEL: &str = "Select a transcription model.";
pub const NO_TEXT_MODEL: &str = "No LLM model selected";
pub const INVALID_KEY: &str = "The API key contains characters that cannot be sent.";
pub const DEPLOYMENT_NEEDS_DATED_VERSION: &str =
    "Deployment-scoped batch transcription requires a dated API version.";

impl Failure {
    pub fn kind(&self) -> &'static str {
        match self {
            Failure::Service { error, .. } => error.kind.name(),
            Failure::NotConfigured(_) => error_kind::NOT_CONFIGURED,
            Failure::KeyWithheld => error_kind::KEY_WITHHELD,
            Failure::InvalidResponse(_) | Failure::Realtime(_) => error_kind::SERVER,
            Failure::Truncated => error_kind::OUTPUT_TRUNCATED,
            Failure::EmptyText => error_kind::EMPTY_RESPONSE,
            Failure::RealtimeTranslation | Failure::RealtimeRejected(_) => {
                error_kind::INVALID_REQUEST
            }
        }
    }

    /// The English text, as in the Windows app where it has one.
    pub fn message(&self) -> String {
        match self {
            Failure::Service { error, .. } => error.message.clone(),
            Failure::NotConfigured(text) | Failure::Realtime(text) => (*text).to_owned(),
            Failure::KeyWithheld => "The server requires the API key, but it is sent only over \
                 HTTPS or to this computer. Use an HTTPS server URL."
                .into(),
            Failure::InvalidResponse(text) => text.clone(),
            Failure::Truncated => {
                "The OpenAI-compatible provider stopped the response at its token limit.".into()
            }
            Failure::EmptyText => "The OpenAI-compatible provider returned no text.".into(),
            Failure::RealtimeTranslation => {
                "Realtime transcription does not support translation. Use batch mode.".into()
            }
            Failure::RealtimeRejected(reason) => {
                format!("The server rejected the live transcription: {reason}")
            }
        }
    }

    pub fn detail(&self) -> Option<String> {
        match self {
            Failure::Service { detail, .. } => detail.clone(),
            Failure::RealtimeRejected(reason) => Some(reason.clone()),
            _ => None,
        }
    }

    /// The JSON-RPC `error` member.
    pub fn to_error(&self) -> Value {
        let data = ErrorData {
            kind: self.kind().to_owned(),
            detail: self.detail(),
        };
        json!({"code": REQUEST_FAILED, "message": self.message(), "data": data})
    }

    /// With the key withheld, a refusal is most likely the missing key.
    pub fn explain_withheld_key(self, withheld: bool) -> Self {
        match self {
            Failure::Service { error, .. }
                if withheld && error.kind == openai::ErrorKind::Authentication =>
            {
                Failure::KeyWithheld
            }
            other => other,
        }
    }
}

impl From<openai::Error> for Failure {
    fn from(error: openai::Error) -> Self {
        Failure::Service {
            error,
            detail: None,
        }
    }
}

impl From<EndpointError> for Failure {
    fn from(error: EndpointError) -> Self {
        Failure::NotConfigured(match error {
            EndpointError::NoServer => NO_SERVER,
            EndpointError::Url(_) => INVALID_URL,
            EndpointError::DeploymentNeedsDatedVersion => DEPLOYMENT_NEEDS_DATED_VERSION,
        })
    }
}

/// No connection, with the system's reason.
pub fn network(reason: impl std::fmt::Display) -> Failure {
    let reason = reason.to_string();
    Failure::Service {
        error: openai::Error {
            kind: openai::ErrorKind::Network,
            message: format!("Network error: {reason}"),
            status: None,
        },
        detail: Some(reason),
    }
}

pub fn timeout() -> Failure {
    Failure::Service {
        error: openai::Error {
            kind: openai::ErrorKind::Timeout,
            message: "API request timed out.".into(),
            status: None,
        },
        detail: None,
    }
}

/// The failure for an HTTP error status, classified like the Windows app does,
/// with the server's own message as the detail.
pub fn from_status(status: u16, body: &str) -> Failure {
    Failure::Service {
        error: openai::status_error(status, body),
        detail: Some(error_detail(body)).filter(|d| !d.is_empty()),
    }
}

/// `error.message`, `error` as a string, `detail` or `message` from a JSON
/// error body, or the first 200 characters of the body.
pub fn error_detail(body: &str) -> String {
    if let Ok(json) = serde_json::from_str::<Value>(body) {
        let found = match &json["error"] {
            Value::Object(error) => error.get("message").and_then(Value::as_str),
            Value::String(error) => Some(error.as_str()),
            _ => None,
        }
        .or_else(|| json["detail"].as_str())
        .or_else(|| json["message"].as_str());
        if let Some(text) = found.filter(|t| !t.trim().is_empty()) {
            return text.trim().to_owned();
        }
    }
    body.trim().chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_servers_message_as_the_detail() {
        let lm_studio =
            r#"{"error":"Unsupported Media Type. POST requests must use 'application/json'"}"#;
        let failure = from_status(415, lm_studio);
        assert_eq!(failure.kind(), error_kind::INVALID_REQUEST);
        assert_eq!(
            failure.detail().as_deref(),
            Some("Unsupported Media Type. POST requests must use 'application/json'")
        );
        assert_eq!(
            failure.to_error()["data"],
            json!({"kind": "invalidRequest", "detail": "Unsupported Media Type. POST requests must use 'application/json'"})
        );
        assert_eq!(from_status(401, "").kind(), error_kind::AUTHENTICATION);
        assert_eq!(from_status(429, "").kind(), error_kind::RATE_LIMIT);
        assert_eq!(from_status(503, "").kind(), error_kind::SERVER);
        assert_eq!(
            from_status(500, r#"{"detail":"Model not loaded"}"#)
                .detail()
                .as_deref(),
            Some("Model not loaded")
        );
        assert_eq!(error_detail(&"x".repeat(300)).len(), 200);
        assert_eq!(error_detail(" plain text "), "plain text");
        assert_eq!(from_status(500, "").detail(), None);
    }

    #[test]
    fn a_refusal_without_the_withheld_key_names_the_reason() {
        assert_eq!(
            from_status(401, "").explain_withheld_key(true),
            Failure::KeyWithheld
        );
        assert_eq!(
            from_status(401, "").explain_withheld_key(false).kind(),
            error_kind::AUTHENTICATION
        );
        assert_eq!(
            from_status(429, "").explain_withheld_key(true).kind(),
            error_kind::RATE_LIMIT
        );
        let error = Failure::KeyWithheld.to_error();
        assert_eq!(error["code"], REQUEST_FAILED);
        assert_eq!(error["data"], json!({"kind": "keyWithheld"}));
    }
}
