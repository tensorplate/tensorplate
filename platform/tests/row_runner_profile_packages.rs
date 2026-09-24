// SPDX-License-Identifier: Apache-2.0
//
// Per-runner-profile package lists on a platform row's `backend_packages`.
// No committed row declares one yet; these cases take the committed L4 row,
// add lists to its `python_pytorch` backend path in memory, and check the
// row schema's verdict.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use serde_json::{json, Value};

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
    assert!(
        validator.is_valid(&l4_row_with(two_profiles())),
        "a row with per-profile package lists must validate"
    );
}

#[test]
fn malformed_per_profile_package_lists_fail_the_row_schema() {
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
    ];
    for (label, lists) in cases {
        assert!(
            !validator.is_valid(&l4_row_with(lists)),
            "{label}: the row schema must reject it"
        );
    }
}
