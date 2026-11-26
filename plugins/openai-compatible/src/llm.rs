//! Text processing through Chat Completions or the Responses API, as in the
//! Windows app.

use std::time::Duration;

use serde_json::{Map, Value, json};
use tw_openai_compatible::server_url::{self, withholds_api_key};
use tw_openai_compatible::{Profile, ReasoningEffort, TextApi, endpoint};

use crate::failure::{self, Failure};
use crate::http;

pub fn process(
    profile: &Profile,
    key: Option<&str>,
    system_prompt: &str,
    user_text: &str,
    model: Option<&str>,
) -> Result<String, Failure> {
    let model = model
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or(profile.selected_llm_model_id.trim());
    if model.is_empty() {
        return Err(Failure::NotConfigured(failure::NO_TEXT_MODEL));
    }
    let timeout = Duration::from_secs(u64::from(profile.text_timeout_seconds()));
    let responses = profile.text_api() == TextApi::Responses;
    let url = if responses {
        endpoint::responses_url(profile)?
    } else {
        endpoint::chat_completions_url(profile)?
    };
    let withheld = withholds_api_key(&url, key);
    let mut body = if responses {
        responses_body(profile, model, system_prompt, user_text)
    } else {
        chat_body(profile, model, system_prompt, user_text)
    };
    let send = |body: &Value| {
        http::post(
            &url,
            key,
            "application/json",
            body.to_string().as_bytes(),
            timeout,
        )
    };
    let mut response = send(&body)?;
    if !responses && wants_other_token_parameter(response.status, &response.body) {
        // Newer OpenAI models take only max_completion_tokens; ask once more with it.
        if let Some(tokens) = body.as_object_mut().and_then(|b| b.remove("max_tokens")) {
            body["max_completion_tokens"] = tokens;
        }
        response = send(&body)?;
    }
    let text = http::success(response).map_err(|f| f.explain_withheld_key(withheld))?;
    if responses {
        parse_responses(&text)
    } else {
        parse_chat(&text)
    }
}

/// Retries only when the server names both parameters in its refusal.
fn wants_other_token_parameter(status: u16, body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    status == 400 && body.contains("max_tokens") && body.contains("max_completion_tokens")
}

pub fn chat_body(profile: &Profile, model: &str, system_prompt: &str, user_text: &str) -> Value {
    let mut body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_text},
        ],
        "max_tokens": output_token_budget(system_prompt, user_text),
    });
    if is_deep_infra(&profile.base_url) {
        // DeepInfra switches reasoning with reasoning_effort only.
        body["reasoning_effort"] = json!(if profile.thinking_enabled {
            "high"
        } else {
            "none"
        });
    } else if profile.thinking_enabled {
        // Nonstandard, so only sent when enabled, as on macOS: OpenAI itself
        // refuses unknown parameters.
        body["thinking"] = json!({"type": "enabled"});
    }
    if let Some(temperature) = profile.temperature() {
        body["temperature"] = json!(temperature);
    }
    body
}

pub fn responses_body(
    profile: &Profile,
    model: &str,
    system_prompt: &str,
    user_text: &str,
) -> Value {
    let effort = profile.reasoning_effort();
    let budget = output_token_budget(system_prompt, user_text);
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert(
        "max_output_tokens".into(),
        json!(if effort == ReasoningEffort::ProviderDefault {
            budget
        } else {
            budget + REASONING_RESERVE_TOKENS
        }),
    );
    let instructions = if system_prompt.trim().is_empty() {
        "You are a helpful assistant."
    } else {
        system_prompt
    };
    body.insert("instructions".into(), json!(instructions));
    body.insert(
        "input".into(),
        json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": user_text}]}]),
    );
    body.insert("store".into(), json!(false));
    if effort != ReasoningEffort::ProviderDefault {
        body.insert("reasoning".into(), json!({"effort": effort.as_str()}));
    }
    if let Some(temperature) = profile.temperature() {
        body.insert("temperature".into(), json!(temperature));
    }
    Value::Object(body)
}

/// `api.deepinfra.com` itself, not a host that only contains the name.
fn is_deep_infra(base_url: &str) -> bool {
    server_url::parse(base_url)
        .is_ok_and(|parts| parts.host.eq_ignore_ascii_case("api.deepinfra.com"))
}

pub const MIN_OUTPUT_TOKENS: u32 = 2048;
pub const MAX_OUTPUT_TOKENS: u32 = 8192;
pub const REASONING_RESERVE_TOKENS: u32 = 25_000;

/// The Windows SDK's output budget: 2048 tokens for short inputs, otherwise
/// the estimated input (four characters a token) plus 2048, in steps of 256,
/// at most 8192.
pub fn output_token_budget(system_prompt: &str, user_text: &str) -> u32 {
    let characters = (system_prompt.chars().count() + user_text.chars().count()) as u64;
    let input_tokens = characters.div_ceil(4);
    if input_tokens <= 1024 {
        return MIN_OUTPUT_TOKENS;
    }
    let desired = (input_tokens + 2048).div_ceil(256) * 256;
    desired.clamp(u64::from(MIN_OUTPUT_TOKENS), u64::from(MAX_OUTPUT_TOKENS)) as u32
}

const TOKEN_LIMIT_REASONS: &[&str] = &[
    "length",
    "max_tokens",
    "max_output_tokens",
    "model_context_window_exceeded",
];

fn is_token_limit(reason: Option<&str>) -> bool {
    reason.is_some_and(|r| {
        TOKEN_LIMIT_REASONS
            .iter()
            .any(|known| r.eq_ignore_ascii_case(known))
    })
}

fn invalid_json() -> Failure {
    Failure::InvalidResponse("The OpenAI-compatible provider returned invalid JSON.".into())
}

pub fn parse_chat(body: &str) -> Result<String, Failure> {
    if body.trim().is_empty() {
        return Err(Failure::EmptyText);
    }
    let json: Value = serde_json::from_str(body).map_err(|_| invalid_json())?;
    let choice = &json["choices"][0];
    if is_token_limit(choice["finish_reason"].as_str())
        || is_token_limit(choice["native_finish_reason"].as_str())
    {
        return Err(Failure::Truncated);
    }
    let content = &choice["message"]["content"];
    let text = match content {
        Value::String(text) => text.trim().to_owned(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<String>()
            .trim()
            .to_owned(),
        _ => String::new(),
    };
    let text = strip_leading_reasoning(&text);
    if text.trim().is_empty() {
        return Err(Failure::EmptyText);
    }
    Ok(text)
}

pub fn parse_responses(body: &str) -> Result<String, Failure> {
    let json: Value = serde_json::from_str(body).map_err(|_| invalid_json())?;
    check_responses_complete(&json)?;
    if matches!(json["status"].as_str(), Some("failed" | "cancelled")) {
        return Err(Failure::InvalidResponse(
            "The provider did not complete the response.".into(),
        ));
    }
    if let Some(direct) = json["output_text"].as_str() {
        let text = strip_leading_reasoning(direct).trim().to_owned();
        if !text.is_empty() {
            return Ok(text);
        }
    }
    let mut parts = String::new();
    for item in json["output"].as_array().into_iter().flatten() {
        for part in item["content"].as_array().into_iter().flatten() {
            if let Some(kind) = part["type"].as_str()
                && kind != "output_text"
                && kind != "text"
            {
                continue;
            }
            match &part["text"] {
                Value::String(text) => parts.push_str(text),
                Value::Object(text) => {
                    if let Some(value) = text.get("value").and_then(Value::as_str) {
                        parts.push_str(value);
                    }
                }
                _ => {}
            }
        }
    }
    let text = strip_leading_reasoning(&parts).trim().to_owned();
    if text.is_empty() {
        return Err(Failure::EmptyText);
    }
    Ok(text)
}

/// An incomplete Responses answer is never passed on as a success.
fn check_responses_complete(json: &Value) -> Result<(), Failure> {
    if json["response"].is_object() {
        check_responses_complete(&json["response"])?;
    }
    let reason = json["incomplete_details"]["reason"].as_str();
    if json["status"]
        .as_str()
        .is_some_and(|s| s.eq_ignore_ascii_case("incomplete"))
    {
        return Err(if is_token_limit(reason) {
            Failure::Truncated
        } else {
            Failure::InvalidResponse(
                "The OpenAI-compatible provider returned an incomplete response.".into(),
            )
        });
    }
    if is_token_limit(reason) {
        return Err(Failure::Truncated);
    }
    Ok(())
}

/// Some servers (Ollama, LM Studio, vLLM without a reasoning parser) put a
/// model's reasoning into the answer. Leading `<think>` and `<thinking>` blocks
/// go; a block whose opening tag the chat template sent goes when its closing
/// tag stands at a line end. A block that never closes stays visible.
pub fn strip_leading_reasoning(text: &str) -> String {
    const BLOCKS: &[(&str, &str)] = &[("<thinking>", "</thinking>"), ("<think>", "</think>")];
    let mut remaining = text.trim_start();
    let mut stripped = false;
    while let Some((open, close)) = BLOCKS
        .iter()
        .find(|(open, _)| starts_with_ignore_case(remaining, open))
    {
        let Some(end) = find_ignore_case(&remaining[open.len()..], close) else {
            return if stripped { remaining } else { text }.to_owned();
        };
        remaining = remaining[open.len() + end + close.len()..].trim_start();
        stripped = true;
    }
    if stripped {
        return remaining.to_owned();
    }
    if BLOCKS
        .iter()
        .any(|(open, _)| find_ignore_case(remaining, open).is_some())
    {
        return text.to_owned();
    }
    match implicit_close(remaining) {
        Some(after) => strip_leading_reasoning(after.trim()),
        None => text.to_owned(),
    }
}

/// The text after the first `</think>` or `</thinking>` that ends its line.
fn implicit_close(text: &str) -> Option<&str> {
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find("</think") {
        let start = from + found + "</think".len();
        let after_name = lower[start..]
            .strip_prefix("ing>")
            .or_else(|| lower[start..].strip_prefix('>'));
        if let Some(rest) = after_name {
            let tag_end = lower.len() - rest.len();
            let line_rest = rest.trim_start_matches([' ', '\t']);
            if line_rest.is_empty() || line_rest.starts_with('\n') || line_rest.starts_with("\r\n")
            {
                return Some(&text[tag_end..]);
            }
        }
        from = start;
    }
    None
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

fn find_ignore_case(text: &str, needle: &str) -> Option<usize> {
    text.to_ascii_lowercase().find(&needle.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> Profile {
        Profile {
            base_url: "https://api.example.com".into(),
            selected_llm_model_id: "gpt-x".into(),
            ..Profile::default()
        }
    }

    #[test]
    fn chat_sends_the_messages_budget_and_optional_controls() {
        let mut p = profile();
        let body = chat_body(&p, "gpt-x", "Be brief.", "Hello");
        assert_eq!(body["model"], "gpt-x");
        assert_eq!(
            body["messages"][0],
            json!({"role": "system", "content": "Be brief."})
        );
        assert_eq!(
            body["messages"][1],
            json!({"role": "user", "content": "Hello"})
        );
        assert_eq!(body["max_tokens"], 2048);
        for absent in ["thinking", "reasoning_effort", "temperature"] {
            assert!(body.get(absent).is_none(), "{absent}");
        }
        p.thinking_enabled = true;
        p.llm_temperature_mode_raw = "custom".into();
        p.llm_temperature_value = 0.2;
        let body = chat_body(&p, "gpt-x", "", "Hi");
        assert_eq!(body["thinking"], json!({"type": "enabled"}));
        assert_eq!(body["temperature"], 0.2);
    }

    #[test]
    fn deep_infra_uses_reasoning_effort_only() {
        let mut p = profile();
        p.base_url = "https://api.deepinfra.com/v1/openai".into();
        let off = chat_body(&p, "m", "", "x");
        assert_eq!(off["reasoning_effort"], "none");
        assert!(off.get("thinking").is_none());
        p.thinking_enabled = true;
        assert_eq!(chat_body(&p, "m", "", "x")["reasoning_effort"], "high");
        p.base_url = "https://api.deepinfra.com.example.net".into();
        let lookalike = chat_body(&p, "m", "", "x");
        assert!(lookalike.get("reasoning_effort").is_none());
        assert_eq!(lookalike["thinking"], json!({"type": "enabled"}));
    }

    #[test]
    fn responses_uses_its_own_shape_and_reasoning_omits_temperature() {
        let mut p = profile();
        p.llm_api_mode_raw = "responses".into();
        p.llm_temperature_mode_raw = "custom".into();
        p.llm_temperature_value = 1.1;
        let body = responses_body(&p, "gpt-x", "", "Hello");
        assert_eq!(body["instructions"], "You are a helpful assistant.");
        assert_eq!(
            body["input"][0]["content"][0],
            json!({"type": "input_text", "text": "Hello"})
        );
        assert_eq!(body["store"], false);
        assert_eq!(body["max_output_tokens"], 2048);
        assert_eq!(body["temperature"], 1.1);
        assert!(body.get("reasoning").is_none());
        assert!(body.get("messages").is_none());
        p.reasoning_effort_raw = "xhigh".into();
        let body = responses_body(&p, "gpt-x", "System", "Hello");
        assert_eq!(body["reasoning"], json!({"effort": "xhigh"}));
        assert_eq!(body["max_output_tokens"], 2048 + REASONING_RESERVE_TOKENS);
        assert!(body.get("temperature").is_none());
        assert_eq!(body["instructions"], "System");
    }

    #[test]
    fn budgets_output_tokens_like_the_windows_sdk() {
        assert_eq!(output_token_budget("", ""), 2048);
        assert_eq!(output_token_budget(&"a".repeat(4096), ""), 2048);
        // 4100 characters are 1025 tokens: 1025 + 2048 rounds up to 3328.
        assert_eq!(output_token_budget(&"a".repeat(4100), ""), 3328);
        assert_eq!(output_token_budget(&"a".repeat(100_000), ""), 8192);
    }

    #[test]
    fn retries_only_the_named_token_parameter_mismatch() {
        let both = r#"{"error":{"message":"Unsupported parameter: 'max_tokens'. Use 'max_completion_tokens' instead."}}"#;
        assert!(wants_other_token_parameter(400, both));
        assert!(!wants_other_token_parameter(
            400,
            r#"{"error":"max_tokens too large"}"#
        ));
        assert!(!wants_other_token_parameter(500, both));
    }

    #[test]
    fn reads_chat_answers() {
        let answer = |content: Value| {
            json!({"choices": [{"message": {"content": content}, "finish_reason": "stop"}]})
                .to_string()
        };
        assert_eq!(parse_chat(&answer(json!(" Hi "))).unwrap(), "Hi");
        assert_eq!(
            parse_chat(&answer(json!([{"type": "text", "text": "a"}, {"type": "image"}, {"type": "text", "text": "b"}])))
                .unwrap(),
            "ab"
        );
        assert_eq!(
            parse_chat(&answer(json!("<think>plan</think>\nanswer"))).unwrap(),
            "answer"
        );
        assert_eq!(
            parse_chat(&answer(json!("<think>only reasoning</think>"))),
            Err(Failure::EmptyText)
        );
        assert_eq!(parse_chat(&answer(json!("   "))), Err(Failure::EmptyText));
        assert_eq!(parse_chat(""), Err(Failure::EmptyText));
        let cut = json!({"choices": [{"message": {"content": "par"}, "finish_reason": "length"}]});
        assert_eq!(parse_chat(&cut.to_string()), Err(Failure::Truncated));
        let native = json!({"choices": [{"message": {"content": "x"}, "native_finish_reason": "MAX_TOKENS"}]});
        assert_eq!(parse_chat(&native.to_string()), Err(Failure::Truncated));
        assert!(matches!(
            parse_chat("<html>"),
            Err(Failure::InvalidResponse(_))
        ));
    }

    #[test]
    fn reads_responses_answers() {
        assert_eq!(
            parse_responses(r#"{"output_text":"direct"}"#).unwrap(),
            "direct"
        );
        let nested = json!({"output": [
            {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "hidden"}]},
            {"type": "message", "content": [
                {"type": "output_text", "text": "a"},
                {"type": "output_text", "text": {"value": "b"}},
                {"text": "c"}
            ]}
        ]});
        assert_eq!(parse_responses(&nested.to_string()).unwrap(), "abc");
        assert_eq!(
            parse_responses(r#"{"output_text":"<think>x</think> answer"}"#).unwrap(),
            "answer"
        );
        assert_eq!(parse_responses(r#"{"output":[]}"#), Err(Failure::EmptyText));
        let incomplete = r#"{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output_text":"partial"}"#;
        assert_eq!(parse_responses(incomplete), Err(Failure::Truncated));
        let filtered = r#"{"status":"incomplete","incomplete_details":{"reason":"content_filter"},"output_text":"partial"}"#;
        assert!(matches!(
            parse_responses(filtered),
            Err(Failure::InvalidResponse(_))
        ));
        let wrapped = r#"{"response":{"status":"incomplete"},"output_text":"partial"}"#;
        assert!(parse_responses(wrapped).is_err());
        assert!(parse_responses(r#"{"status":"failed","output_text":"x"}"#).is_err());
    }

    #[test]
    fn removes_leading_inline_reasoning() {
        for (text, expected) in [
            ("<think>a</think>b", "b"),
            ("  <THINK>a</THINK>\n\n<thinking>b</thinking> c", "c"),
            ("<think>never closed", "<think>never closed"),
            ("<think>a</think><think>b", "<think>b"),
            ("plan from the template</think>\nanswer", "answer"),
            ("plan</thinking>  \r\nanswer", "answer"),
            (
                "Use </think> to close a block.",
                "Use </think> to close a block.",
            ),
            ("no reasoning", "no reasoning"),
            ("text <think>x</think>", "text <think>x</think>"),
        ] {
            assert_eq!(strip_leading_reasoning(text), expected, "{text:?}");
        }
    }
}
