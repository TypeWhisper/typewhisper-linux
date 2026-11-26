//! Server URLs: what a profile may store, and where its API key may go.

use std::net::{Ipv4Addr, Ipv6Addr};

/// Why a server URL cannot be stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlError {
    /// Not `http://` or `https://`.
    Scheme,
    /// A user name or password before the host, which would end up in logs.
    Credentials,
    /// A query or fragment; the API version has its own field.
    QueryOrFragment,
    /// No host, or one with characters a host name cannot have.
    Host,
    Port,
    /// Spaces, control characters or backslashes.
    Characters,
}

/// The parts of a URL that decide where a request goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parts<'a> {
    /// In lower case.
    pub scheme: String,
    /// Without the brackets of an IPv6 address.
    pub host: &'a str,
    pub port: Option<u16>,
    /// Everything after the host and port, starting with `/`, or empty.
    pub rest: &'a str,
}

/// Splits `scheme://host[:port][/path][?query][#fragment]`.
pub fn parse(url: &str) -> Result<Parts<'_>, UrlError> {
    if url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return Err(UrlError::Characters);
    }
    let (scheme, after) = url.split_once("://").ok_or(UrlError::Scheme)?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        return Err(UrlError::Scheme);
    }
    let end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let (authority, rest) = after.split_at(end);
    if authority.contains('@') {
        return Err(UrlError::Credentials);
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (address, after) = bracketed.split_once(']').ok_or(UrlError::Host)?;
        address.parse::<Ipv6Addr>().map_err(|_| UrlError::Host)?;
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':').ok_or(UrlError::Port)?),
        };
        (address, port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let valid_host = !host.is_empty()
        && (host.contains(':')
            || host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._".contains(c)));
    if !valid_host {
        return Err(UrlError::Host);
    }
    let port = match port {
        None => None,
        Some(digits) if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) => Some(
            digits
                .parse::<u16>()
                .ok()
                .filter(|p| *p > 0)
                .ok_or(UrlError::Port)?,
        ),
        Some(_) => return Err(UrlError::Port),
    };
    Ok(Parts {
        scheme: scheme.to_ascii_lowercase(),
        host,
        port,
        rest,
    })
}

/// Trims the URL and drops trailing slashes and a trailing `/v1`, which every
/// request adds again, as on macOS and Windows.
pub fn normalize(url: &str) -> String {
    let mut url = url.trim().trim_end_matches('/');
    // `get` stays on character boundaries, so a URL ending in "é" is no panic.
    let ends_in_v1 = url
        .get(url.len().saturating_sub(3)..)
        .is_some_and(|end| end.eq_ignore_ascii_case("/v1"));
    if ends_in_v1 {
        url = url[..url.len() - 3].trim_end_matches('/');
    }
    match url.split_once("://") {
        Some((scheme, rest)) => format!("{}://{rest}", scheme.to_ascii_lowercase()),
        None => url.to_owned(),
    }
}

/// The normalized URL, or why it cannot be stored. An empty URL is allowed and
/// leaves the profile unconfigured.
pub fn validate(url: &str) -> Result<String, UrlError> {
    let url = normalize(url);
    if url.is_empty() {
        return Ok(url);
    }
    let scheme = url.split_once("://").map(|(scheme, _)| scheme);
    if !scheme.is_some_and(|s| s.eq_ignore_ascii_case("http") || s.eq_ignore_ascii_case("https")) {
        return Err(UrlError::Scheme);
    }
    let parts = parse(&url)?;
    if parts.rest.contains(['?', '#']) {
        return Err(UrlError::QueryOrFragment);
    }
    Ok(url)
}

/// True for `localhost`, `127.0.0.0/8` and `::1`.
pub fn is_this_computer(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(address) = host.parse::<Ipv4Addr>() {
        return address.is_loopback();
    }
    match host.parse::<Ipv6Addr>() {
        Ok(address) => {
            address.is_loopback() || address.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    }
}

/// An API key is a bearer secret, so it travels only over TLS or to this
/// computer. A server on another computer over plain HTTP, such as Ollama or
/// LM Studio in the local network, gets its requests without the key, so
/// servers that need none keep working. The rule of the Windows app.
pub fn sends_api_key_to(url: &str) -> bool {
    match parse(url) {
        Ok(parts) => match parts.scheme.as_str() {
            "https" | "wss" => true,
            "http" | "ws" => is_this_computer(parts.host),
            _ => false,
        },
        Err(_) => false,
    }
}

/// True when a key is set but the rule keeps it from `url`.
pub fn withholds_api_key(url: &str, key: Option<&str>) -> bool {
    key.is_some_and(|k| !k.trim().is_empty()) && !sends_api_key_to(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sends_the_key_only_over_tls_or_to_this_computer() {
        for (url, sends) in [
            ("https://api.example.com/v1/models", true),
            ("wss://api.example.com/v1/realtime", true),
            ("http://localhost:11434/v1/models", true),
            ("HTTP://LocalHost:11434/v1/models", true),
            ("http://127.0.0.1:11434/v1/models", true),
            ("http://127.1.2.3/v1/models", true),
            ("http://[::1]:11434/v1/models", true),
            ("http://[::ffff:127.0.0.1]/v1/models", true),
            ("ws://localhost:8080/v1/realtime", true),
            ("http://192.168.1.5:11434/v1/models", false),
            ("http://service.local:1234/v1/models", false),
            ("http://host.lima.internal:1234/v1/models", false),
            ("ws://192.168.1.5:8080/v1/realtime", false),
            ("http://localhost.example.com/v1/models", false),
            ("http://[::2]/v1/models", false),
            ("ftp://localhost/v1/models", false),
            ("http://localhost@evil.example/v1/models", false),
            ("not a url", false),
        ] {
            assert_eq!(sends_api_key_to(url), sends, "{url}");
            assert_eq!(withholds_api_key(url, Some("secret")), !sends, "{url}");
            assert!(!withholds_api_key(url, None));
            assert!(!withholds_api_key(url, Some("  ")));
        }
    }

    #[test]
    fn normalizes_trailing_slashes_and_v1() {
        for (input, expected) in [
            ("  http://localhost:11434/v1/  ", "http://localhost:11434"),
            (
                "https://api.groq.com/openai/v1",
                "https://api.groq.com/openai",
            ),
            (
                "https://api.groq.com/openai/V1",
                "https://api.groq.com/openai",
            ),
            ("HTTPS://Example.com//", "https://Example.com"),
            (
                "https://unit.openai.azure.com/openai",
                "https://unit.openai.azure.com/openai",
            ),
            ("https://example.com/v10", "https://example.com/v10"),
            ("", ""),
            ("https://example.com/éé", "https://example.com/éé"),
            ("/v1", ""),
            ("é", "é"),
        ] {
            assert_eq!(normalize(input), expected, "{input}");
        }
    }

    #[test]
    fn rejects_urls_a_profile_cannot_store() {
        for (url, error) in [
            ("file:///tmp/test", UrlError::Scheme),
            ("ftp://example.com", UrlError::Scheme),
            ("example.com", UrlError::Scheme),
            ("https://user:secret@example.com", UrlError::Credentials),
            ("https://example.com?key=secret", UrlError::QueryOrFragment),
            (
                "https://example.com/v1?key=secret",
                UrlError::QueryOrFragment,
            ),
            ("https://example.com#fragment", UrlError::QueryOrFragment),
            ("https://exa mple.com", UrlError::Characters),
            ("https://example.com\\@evil", UrlError::Characters),
            ("https://", UrlError::Scheme),
            ("https:///path", UrlError::Host),
            ("https://exa!mple.com", UrlError::Host),
            ("https://example.com:http", UrlError::Port),
            ("https://example.com:0", UrlError::Port),
            ("https://example.com:70000", UrlError::Port),
            ("https://[::1", UrlError::Host),
            ("https://[nonsense]", UrlError::Host),
        ] {
            assert_eq!(validate(url), Err(error), "{url}");
        }
        assert_eq!(
            validate("http://[::1]:8080/v1/").as_deref(),
            Ok("http://[::1]:8080")
        );
        assert_eq!(validate("  ").as_deref(), Ok(""));
    }

    #[test]
    fn parses_host_and_port() {
        let parts = parse("https://[::1]:8443/openai").unwrap();
        assert_eq!(
            (parts.host, parts.port, parts.rest),
            ("::1", Some(8443), "/openai")
        );
        let parts = parse("http://host.lima.internal:1234").unwrap();
        assert_eq!(
            (parts.host, parts.port, parts.rest),
            ("host.lima.internal", Some(1234), "")
        );
    }
}
