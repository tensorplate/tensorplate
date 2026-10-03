// SPDX-License-Identifier: Apache-2.0
//
// Runner profile declarations: the files the speech runtime's profile
// packages install beside `backend.json`. The packaged declarations must
// validate against `definitions.runner_profile_declaration` of
// `protocol/schemas/backend_descriptor.json`, and
// `fixtures/runner_profile_declarations/cases.json` lists what each install
// combination leaves on disk.

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

fn read_json(relative: &str) -> Value {
    serde_json::from_str(&read(relative)).unwrap_or_else(|e| panic!("{relative}: {e}"))
}

const SCHEMA: &str = "protocol/schemas/backend_descriptor.json";
const MERGED: &str = "protocol/rust/tests/fixtures/backend_descriptor_runner_profiles.json";
const CASES: &str = "protocol/rust/tests/fixtures/runner_profile_declarations/cases.json";
const SECOND: &str =
    "protocol/rust/tests/fixtures/runner_profile_declarations/second_faster_whisper.json";
const STT: &str = "packaging/backend-metadata/runner_profiles/faster_whisper.json";
const TTS: &str = "packaging/backend-metadata/runner_profiles/kokoro.json";

/// A validator for one declaration document.
fn declaration_validator() -> jsonschema::JSONSchema {
    let schema = read_json(SCHEMA);
    let id = schema["$id"].as_str().expect("$id").to_owned();
    let mut options = jsonschema::JSONSchema::options();
    options.with_document(id.clone(), schema);
    options
        .compile(&json!({
            "$ref": format!("{id}#/definitions/runner_profile_declaration")
        }))
        .expect("the declaration definition compiles as Draft-07")
}

/// The packaged `faster_whisper` declaration with one member replaced, or
/// removed when `value` is `None`.
fn stt_with(pointer: &str, value: Option<Value>) -> Value {
    let mut doc = read_json(STT);
    let (parent, key) = pointer.rsplit_once('/').expect("pointer");
    let parent = if parent.is_empty() {
        &mut doc
    } else {
        doc.pointer_mut(parent).expect("parent")
    };
    let object = parent.as_object_mut().expect("object");
    match value {
        Some(v) => {
            object.insert(key.to_owned(), v);
        }
        None => {
            object.remove(key).expect("member to remove");
        }
    }
    doc
}

/// Variants the schema must refuse.
fn schema_refused_cases() -> Vec<(&'static str, Value)> {
    vec![
        ("an array", json!([read_json(STT)])),
        ("missing schema_version", stt_with("/schema_version", None)),
        (
            "null schema_version",
            stt_with("/schema_version", Some(Value::Null)),
        ),
        (
            "another schema_version",
            stt_with("/schema_version", Some(json!("0.2"))),
        ),
        ("missing backend_name", stt_with("/backend_name", None)),
        (
            "empty backend_name",
            stt_with("/backend_name", Some(json!(""))),
        ),
        ("missing runner_profile", stt_with("/runner_profile", None)),
        (
            "a list of profiles",
            stt_with(
                "/runner_profile",
                Some(json!([read_json(STT)["runner_profile"]])),
            ),
        ),
        (
            "an unknown member",
            stt_with(
                "/package_name",
                Some(json!("tensorplate-speech-runtime-ct2")),
            ),
        ),
        (
            "a descriptor's runner_profiles list",
            stt_with(
                "/runner_profiles",
                Some(json!([read_json(STT)["runner_profile"]])),
            ),
        ),
        (
            "an unknown profile member",
            stt_with("/runner_profile/module", Some(json!("faster_whisper"))),
        ),
        (
            "a relative interpreter",
            stt_with("/runner_profile/interpreter", Some(json!("bin/python"))),
        ),
        (
            "an unknown compute type",
            stt_with("/runner_profile/compute_types", Some(json!(["auto"]))),
        ),
        (
            "no package",
            stt_with("/runner_profile/packages", Some(json!([]))),
        ),
    ]
}

#[test]
fn the_packaged_declarations_validate() {
    let validator = declaration_validator();
    for relative in [STT, TTS, SECOND] {
        assert!(
            validator.is_valid(&read_json(relative)),
            "{relative} must validate as a runner profile declaration"
        );
    }
}

#[test]
fn the_schema_refuses_malformed_declarations() {
    let validator = declaration_validator();
    for (name, instance) in schema_refused_cases() {
        assert!(!validator.is_valid(&instance), "the schema accepted {name}");
    }
}

#[test]
fn a_declaration_holds_the_descriptors_own_entry() {
    // One entry shape: a declaration cannot say what `runner_profiles` cannot.
    let schema = read_json(SCHEMA);
    assert_eq!(
        schema["definitions"]["runner_profile_declaration"]["properties"]["runner_profile"],
        json!({"$ref": "#/properties/runner_profiles/items"})
    );
}

#[test]
fn the_packaged_declarations_are_the_merged_fixtures_entries() {
    // The merged-view fixture other tests resolve against stays what the
    // packages install.
    let declared: Vec<Value> = [STT, TTS]
        .iter()
        .map(|relative| read_json(relative)["runner_profile"].clone())
        .collect();
    assert_eq!(read_json(MERGED)["runner_profiles"], json!(declared));
}

#[test]
fn every_case_names_files_that_exist() {
    let cases = read_json(CASES);
    let cases = cases["cases"].as_array().expect("cases");
    assert!(!cases.is_empty());
    for case in cases {
        let name = case["name"].as_str().expect("name");
        assert_eq!(
            case.get("runner_profiles").is_some(),
            case.get("refusal").is_none(),
            "{name}: expects profiles or a refusal, not both"
        );
        let Some(declarations) = case["declarations"].as_object() else {
            assert!(case["declarations"].is_null(), "{name}: declarations");
            continue;
        };
        for source in declarations.values() {
            let source = source.as_str().expect("source path");
            assert!(repo_path(source).is_file(), "{name}: {source} is missing");
        }
    }
}
