// SPDX-License-Identifier: Apache-2.0
//
// Backend descriptor fixtures. The committed descriptor with runner
// profiles and the descriptor shipped in `packaging/backend-metadata/` must
// validate against `protocol/schemas/backend_descriptor.json`, and each
// malformed variant of the fixture must fail it.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use serde_json::{json, Value};

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn read(relative: &str) -> String {
    let path = repo_path(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn schema_document() -> Value {
    serde_json::from_str(&read("protocol/schemas/backend_descriptor.json"))
        .expect("schema document parses")
}

const FIXTURE: &str = "protocol/rust/tests/fixtures/backend_descriptor_runner_profiles.json";
const SHIPPED: &str = "packaging/backend-metadata/python_pytorch.json";

fn fixture() -> Value {
    serde_json::from_str(&read(FIXTURE)).expect("fixture parses as JSON")
}

/// The fixture with one runner profile field replaced (or removed when
/// `value` is `None`).
fn with_profile_field(profile: usize, field: &str, value: Option<Value>) -> Value {
    let mut doc = fixture();
    let entry = doc["runner_profiles"][profile]
        .as_object_mut()
        .expect("runner profile entry is an object");
    match value {
        Some(v) => {
            entry.insert(field.to_string(), v);
        }
        None => {
            entry.remove(field);
        }
    }
    doc
}

#[test]
fn committed_descriptors_validate_against_the_schema() {
    let validator =
        jsonschema::JSONSchema::compile(&schema_document()).expect("schema compiles as Draft-07");
    for relative in [FIXTURE, SHIPPED] {
        let instance: Value = serde_json::from_str(&read(relative)).expect("parses as JSON");
        assert!(
            validator.is_valid(&instance),
            "{relative} must validate against the backend descriptor schema"
        );
    }
}

#[test]
fn the_fixture_declares_a_faster_whisper_and_a_kokoro_profile() {
    let doc = fixture();
    let ids: Vec<&str> = doc["runner_profiles"]
        .as_array()
        .expect("runner_profiles is an array")
        .iter()
        .map(|p| p["id"].as_str().expect("id is a string"))
        .collect();
    assert_eq!(ids, ["faster_whisper", "kokoro"]);
}

#[test]
fn malformed_runner_profiles_fail_the_schema() {
    let validator =
        jsonschema::JSONSchema::compile(&schema_document()).expect("schema compiles as Draft-07");
    let cases: Vec<(&str, Value)> = vec![
        (
            "relative interpreter",
            with_profile_field(0, "interpreter", Some(json!("bin/python"))),
        ),
        (
            "relative environment root",
            with_profile_field(0, "environment_root", Some(json!("usr/lib/x"))),
        ),
        (
            "relative library search path",
            with_profile_field(0, "library_search_paths", Some(json!(["lib"]))),
        ),
        (
            "unknown compute type",
            with_profile_field(0, "compute_types", Some(json!(["float8"]))),
        ),
        (
            "auto is not a compute type",
            with_profile_field(0, "compute_types", Some(json!(["auto"]))),
        ),
        (
            "empty compute types",
            with_profile_field(0, "compute_types", Some(json!([]))),
        ),
        (
            "repeated compute type",
            with_profile_field(0, "compute_types", Some(json!(["float16", "float16"]))),
        ),
        (
            "empty packages",
            with_profile_field(1, "packages", Some(json!([]))),
        ),
        (
            "blank package name",
            with_profile_field(1, "packages", Some(json!([""]))),
        ),
        (
            "id not lower_snake_case",
            with_profile_field(1, "id", Some(json!("Kokoro"))),
        ),
        ("missing interpreter", with_profile_field(0, "interpreter", None)),
        ("missing packages", with_profile_field(1, "packages", None)),
        ("missing compute types", with_profile_field(1, "compute_types", None)),
        (
            "unknown field in an entry",
            with_profile_field(0, "python_path", Some(json!("/usr/bin/python3"))),
        ),
        (
            "null library search paths",
            with_profile_field(0, "library_search_paths", Some(Value::Null)),
        ),
        ("null runner_profiles", {
            let mut doc = fixture();
            doc["runner_profiles"] = Value::Null;
            doc
        }),
    ];
    for (label, instance) in cases {
        assert!(
            !validator.is_valid(&instance),
            "{label}: the schema must reject it"
        );
    }
}
