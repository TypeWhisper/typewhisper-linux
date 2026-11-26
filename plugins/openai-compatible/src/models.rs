//! The connection test and model discovery, both through `/v1/models`, with
//! the values in the editor rather than the saved ones.

use std::time::Duration;

use serde_json::Value;
use tw_openai_compatible::profile::normalize_models;
use tw_openai_compatible::server_url::withholds_api_key;
use tw_openai_compatible::{FetchedModel, Profile, endpoint};

use crate::failure::Failure;
use crate::http;

/// Windows gives editor actions 15 seconds.
pub const TIMEOUT: Duration = Duration::from_secs(15);

/// What the server answered to `/v1/models`.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    pub models: Vec<FetchedModel>,
    /// The profile has a key, but the key rule kept it from this server.
    pub key_withheld: bool,
}

pub fn fetch(profile: &Profile, key: Option<&str>) -> Result<Catalog, Failure> {
    let url = endpoint::models_url(profile)?;
    let key_withheld = withholds_api_key(&url, key);
    let body = http::success(http::get(&url, key, TIMEOUT)?)
        .map_err(|f| f.explain_withheld_key(key_withheld))?;
    Ok(Catalog {
        models: parse(&body),
        key_withheld,
    })
}

/// The entries of `data` that have a string `id`, without repeats and sorted.
/// Anything else gives an empty list, and models can still be entered by hand.
pub fn parse(body: &str) -> Vec<FetchedModel> {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let models = json["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some(FetchedModel {
                id: entry.get("id")?.as_str()?.to_owned(),
                owned_by: entry
                    .get("owned_by")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect();
    normalize_models(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_valid_entries_of_the_catalog() {
        let body = r#"{"object":"list","data":[
            {"id":"whisper-large-v3-turbo","owned_by":"OpenAI"},
            {"id":"google/gemma-4-e4b","object":"model"},
            {"id":"Whisper-Large-V3-Turbo"},
            {"id":7}, "text", {"name":"no id"}, {"id":"  "}
        ]}"#;
        let models = parse(body);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["google/gemma-4-e4b", "whisper-large-v3-turbo"]);
        assert_eq!(models[1].owned_by.as_deref(), Some("OpenAI"));
        for empty in ["", "[]", r#"{"data":{}}"#, r#"{"models":[]}"#] {
            assert!(parse(empty).is_empty(), "{empty}");
        }
    }
}
