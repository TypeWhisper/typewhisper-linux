//! HTTP requests with the profile's key, under the key rule.

use std::time::Duration;

use tw_openai_compatible::endpoint::authentication_headers;

use crate::failure::{self, Failure};

/// What the server answered, whatever the status.
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// Largest answer read into memory: model lists and transcripts are far smaller.
const MAX_BODY: u64 = 16 * 1024 * 1024;

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .user_agent(concat!("TypeWhisper/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

pub fn get(url: &str, key: Option<&str>, timeout: Duration) -> Result<Response, Failure> {
    let mut request = agent()
        .get(url)
        .config()
        .timeout_global(Some(timeout))
        .build();
    for (name, value) in authentication_headers(url, key) {
        request = request.header(name, value);
    }
    finish(request.call())
}

pub fn post(
    url: &str,
    key: Option<&str>,
    content_type: &str,
    body: &[u8],
    timeout: Duration,
) -> Result<Response, Failure> {
    let mut request = agent()
        .post(url)
        .config()
        .timeout_global(Some(timeout))
        .build()
        .header("Content-Type", content_type);
    for (name, value) in authentication_headers(url, key) {
        request = request.header(name, value);
    }
    finish(request.send(body))
}

fn finish(
    result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<Response, Failure> {
    let response = result.map_err(from_transport)?;
    let status = response.status().as_u16();
    let body = response
        .into_body()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .map_err(from_transport)?;
    Ok(Response { status, body })
}

/// The body of a successful answer, or the failure for its status.
pub fn success(response: Response) -> Result<String, Failure> {
    if (200..300).contains(&response.status) {
        Ok(response.body)
    } else {
        Err(failure::from_status(response.status, &response.body))
    }
}

fn from_transport(error: ureq::Error) -> Failure {
    match error {
        ureq::Error::Timeout(_) => failure::timeout(),
        ureq::Error::Io(e) if e.kind() == std::io::ErrorKind::TimedOut => failure::timeout(),
        other => failure::network(other),
    }
}
