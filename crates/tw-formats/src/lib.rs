//! Shared TypeWhisper data formats (spike): the `typewhisper-sync` operation
//! envelope and the item identities, exactly as the Mac app defines them in
//! `UserDataSync.swift`, `CloudFolderSyncEngine.swift` and `UserDataSyncIdentity.swift`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

pub const REGEX_CORRECTION_PREFIX: &str = "dictionary:correction:regex:";

/// One immutable operation file below `ops/<device-id>/`.
/// Unknown fields are kept, so a newer writer's data survives a read and write.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SyncOperation {
    pub schema_version: u32,
    pub operation_id: String,
    pub device_id: String,
    pub collection: String,
    pub item_id: String,
    pub kind: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dictionary: Option<DictionaryEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<Snippet>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DictionaryEntry {
    pub entry_type: String,
    pub original: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
    pub case_sensitive: bool,
    pub is_enabled: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_regex: bool,
    pub created_at: String,
    pub updated_at: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Snippet {
    pub trigger: String,
    pub replacement: String,
    pub case_sensitive: bool,
    pub is_enabled: bool,
    /// Older Mac versions write no tags; they mean an empty list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Trim, fold case and diacritics like Foundation's
/// `folding([.caseInsensitive, .diacriticInsensitive], locale: en_US_POSIX)`, then NFC.
pub fn normalized_key(value: &str) -> String {
    let trimmed = value.trim();
    let folded = caseless::default_case_fold_str(&trimmed.nfd().collect::<String>());
    folded
        .nfd()
        .filter(|c| !is_combining_mark(*c))
        .nfc()
        .collect()
}

fn encoded_key(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(value.as_bytes())
}

pub fn dictionary_item_id(entry_type: &str, original: &str, is_regex: bool) -> String {
    if entry_type == "correction" && is_regex {
        // Pattern bytes are significant, exactly as on the Mac.
        return format!("{REGEX_CORRECTION_PREFIX}{}", encoded_key(original));
    }
    format!(
        "dictionary:{entry_type}:{}",
        encoded_key(&normalized_key(original))
    )
}

pub fn snippet_item_id(trigger: &str) -> String {
    format!("snippet:{}", encoded_key(&normalized_key(trigger)))
}
