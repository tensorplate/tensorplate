// SPDX-License-Identifier: Apache-2.0
//
// Per-runner-profile package lists on a platform row's `backend_packages`.
// No committed row declares one yet; these cases take the committed L4 row,
// add lists to its `python_pytorch` backend path in memory, and require the
// row schema and the Rust decoder to agree. The rules only the decoder can
// state are listed separately with the schema's verdict pinned.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use serde_json::{json, Value};
use tensorplate_platform::PlatformSupportRow;

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

fn read(relative: &str) -> String {
    let path = repo_path(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn row_schema() -> Value {
    serde_json::from_str(&read("config/schemas/platform_support_row.json"))
        .expect("row schema parses")
}

const L4_ROW: &str = "config/platform/rows/ubuntu2404-x86-l4-g2s8.json";

/// The committed L4 row with `runner_profiles` set on its `python_pytorch`
/// backend path.
fn l4_row_with(runner_profiles: Value) -> Value {
    let mut row: Value = serde_json::from_str(&read(L4_ROW)).expect("row parses as JSON");
    let sets = row["backend_packages"]
        .as_array_mut()
        .expect("backend_packages is an array");
    let set = sets
        .iter_mut()
        .find(|s| s["backend_path"] == "python_pytorch")
        .expect("the L4 row declares the python_pytorch backend path");
    set.as_object_mut()
        .expect("backend package set is an object")
        .insert("runner_profiles".to_string(), runner_profiles);
    row
}

fn decodes(row: &Value) -> Option<PlatformSupportRow> {
    PlatformSupportRow::from_json(&serde_json::to_string(row).expect("serialize")).ok()
}

fn two_profiles() -> Value {
    json!([
        {
            "runner_profile": "faster_whisper",
            "packages": [
                "tensorplate-speech-runtime-base",
                "tensorplate-speech-runtime-ct2",
                "tensorplate-speech-runtime-vad",
                "tensorplate-speech-runtime-cublas"
            ]
        },
        {
            "runner_profile": "kokoro",
            "packages": [
                "tensorplate-speech-runtime-base",
                "tensorplate-speech-runtime-kokoro",
                "tensorplate-speech-runtime-torch",
                "tensorplate-speech-runtime-cublas",
                "tensorplate-speech-runtime-cuda"
            ]
        }
    ])
}

#[test]
fn per_profile_package_lists_validate_against_the_row_schema() {
    let validator = jsonschema::JSONSchema::compile(&row_schema()).expect("row schema compiles");
    let row = l4_row_with(two_profiles());
    assert!(
        validator.is_valid(&row),
        "a row with per-profile package lists must validate"
    );
    let decoded = decodes(&row).expect("a row with per-profile package lists must decode");
    let set = decoded
        .backend_packages()
        .iter()
        .find(|s| s.backend_path == "python_pytorch")
        .expect("python_pytorch backend path");
    let profiles: Vec<&str> = set
        .runner_profiles
        .iter()
        .map(|p| p.runner_profile.as_str())
        .collect();
    assert_eq!(profiles, ["faster_whisper", "kokoro"]);
    // Round-trips through the same validated path.
    let again = PlatformSupportRow::from_json(&serde_json::to_string(&decoded).expect("serialize"))
        .expect("re-decode");
    assert_eq!(decoded, again);
}

#[test]
fn committed_rows_declare_no_per_profile_lists() {
    // No package installs a speech runner profile yet, so no committed row
    // requires one.
    let dir = repo_path("config/platform/rows");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("rows directory") {
        let path = entry.expect("row entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("row reads");
        let row = PlatformSupportRow::from_json(&body)
            .unwrap_or_else(|e| panic!("{} decodes: {e}", path.display()));
        assert!(
            row.backend_packages()
                .iter()
                .all(|set| set.runner_profiles.is_empty()),
            "{} declares a per-runner-profile package list",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "no committed row was read from {}",
        dir.display()
    );
}

#[test]
fn malformed_per_profile_package_lists_are_refused_by_the_schema_and_the_decoder() {
    let validator = jsonschema::JSONSchema::compile(&row_schema()).expect("row schema compiles");
    let cases: Vec<(&str, Value)> = vec![
        (
            "empty package list",
            json!([{"runner_profile": "kokoro", "packages": []}]),
        ),
        (
            "blank package name",
            json!([{"runner_profile": "kokoro", "packages": [""]}]),
        ),
        (
            "repeated package",
            json!([{"runner_profile": "kokoro", "packages": ["a", "a"]}]),
        ),
        (
            "profile id not lower_snake_case",
            json!([{"runner_profile": "Kokoro", "packages": ["a"]}]),
        ),
        ("missing packages", json!([{"runner_profile": "kokoro"}])),
        ("missing profile id", json!([{"packages": ["a"]}])),
        (
            "unknown field",
            json!([{"runner_profile": "kokoro", "packages": ["a"], "channel": "apt"}]),
        ),
        ("null", Value::Null),
        ("an entry written as an array", json!([["kokoro", ["a"]]])),
    ];
    for (label, lists) in cases {
        let row = l4_row_with(lists);
        assert!(
            !validator.is_valid(&row),
            "{label}: the row schema must refuse it"
        );
        assert!(
            decodes(&row).is_none(),
            "{label}: the decoder must refuse it"
        );
    }
}

#[test]
fn rules_only_the_decoder_can_state_are_enforced_by_it() {
    // The schema accepts each of these; the decoder refuses them, and for
    // the reason named.
    let validator = jsonschema::JSONSchema::compile(&row_schema()).expect("row schema compiles");
    let cases = [
        (
            "a profile listed twice on one path",
            json!([
                {"runner_profile": "kokoro", "packages": ["a"]},
                {"runner_profile": "kokoro", "packages": ["b"]}
            ]),
            "must not list a runner profile twice",
        ),
        (
            "a blank-only package name",
            json!([{"runner_profile": "kokoro", "packages": [" "]}]),
            "at least one non-empty package",
        ),
    ];
    for (label, lists, reason) in cases {
        let row = l4_row_with(lists);
        assert!(
            validator.is_valid(&row),
            "{label}: the schema accepts it, so the decoder is the only guard"
        );
        let refused =
            PlatformSupportRow::from_json(&serde_json::to_string(&row).expect("serialize"))
                .err()
                .map_or_else(
                    || panic!("{label}: the decoder must refuse it"),
                    |e| e.to_string(),
                );
        assert!(
            refused.contains(reason),
            "{label}: refused for another reason: {refused}"
        );
    }
}
