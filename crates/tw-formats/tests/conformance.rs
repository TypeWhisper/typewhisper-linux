use serde_json::Value;
use std::fs;
use tw_formats::*;

fn fixture(name: &str) -> String {
    fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[test]
fn mac_operations_round_trip_unchanged() {
    for name in [
        "upsert-dictionary-v1.json",
        "upsert-dictionary-ctc-v1.json",
        "upsert-dictionary-regex-v1.json",
        "upsert-dictionary-regex-v2.json",
        "upsert-snippet-legacy-v1.json",
    ] {
        let text = fixture(name);
        let op: SyncOperation =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let original: Value = serde_json::from_str(&text).unwrap();
        let written = serde_json::to_value(&op).unwrap();
        assert_eq!(original, written, "{name} changed on round trip");
    }
}

#[test]
fn regex_identities_match_mac() {
    let rows: Vec<Value> =
        serde_json::from_str(&fixture("regex-dictionary-identities-v2.json")).unwrap();
    for row in rows {
        let pattern = row["pattern"].as_str().unwrap();
        assert_eq!(
            dictionary_item_id("correction", pattern, true),
            row["itemId"].as_str().unwrap(),
            "pattern {pattern:?}"
        );
    }
}

#[test]
fn identities_match_foundation_vectors() {
    let rows: Vec<Value> = serde_json::from_str(&fixture("identity-vectors.json")).unwrap();
    let mut failures = Vec::new();
    for row in &rows {
        let input = row["input"].as_str().unwrap();
        let expected = row["normalized"].as_str().unwrap();
        let got = normalized_key(input);
        if got != expected {
            failures.push(format!("{input:?}: expected {expected:?}, got {got:?}"));
            continue;
        }
        assert_eq!(
            dictionary_item_id("term", input, false),
            row["termId"].as_str().unwrap()
        );
        assert_eq!(snippet_item_id(input), row["snippetId"].as_str().unwrap());
        assert_eq!(
            dictionary_item_id("correction", input, true),
            row["regexId"].as_str().unwrap()
        );
    }
    assert!(
        failures.is_empty(),
        "{} of {} differ:\n{}",
        failures.len(),
        rows.len(),
        failures.join("\n")
    );
}
