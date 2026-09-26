// SPDX-License-Identifier: Apache-2.0
//
// Backend descriptor fixtures. The committed descriptor with runner
// profiles and the descriptor shipped in `packaging/backend-metadata/` must
// validate against `protocol/schemas/backend_descriptor.json` and parse in
// the Rust mirror. Every malformed variant of the fixture must be refused by
// both; the rules only the Rust reader can state are listed separately with
// the schema's verdict pinned, so a change on either side shows up here.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tensorplate_protocol::backend_descriptor::{BackendDescriptor, ComputeType};

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
const ROOT: &str = "/usr/lib/tensorplate/speech-runtime";

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

fn rust_accepts(instance: &Value) -> bool {
    reader_error(instance).is_none()
}

/// Why the reader refuses `instance`, or `None` when it accepts it.
fn reader_error(instance: &Value) -> Option<String> {
    BackendDescriptor::parse_with_path(
        &serde_json::to_string(instance).expect("serialize"),
        Path::new(FIXTURE),
    )
    .err()
    .map(|err| err.to_string())
}

#[test]
fn committed_descriptors_validate_and_parse() {
    let validator =
        jsonschema::JSONSchema::compile(&schema_document()).expect("schema compiles as Draft-07");
    for relative in [FIXTURE, SHIPPED] {
        let instance: Value = serde_json::from_str(&read(relative)).expect("parses as JSON");
        assert!(
            validator.is_valid(&instance),
            "{relative} must validate against the backend descriptor schema"
        );
        let parsed = BackendDescriptor::parse_with_path(&read(relative), &repo_path(relative))
            .unwrap_or_else(|e| panic!("{relative} must parse: {e}"));
        let again = BackendDescriptor::parse_with_path(
            &serde_json::to_string(&parsed).expect("serialize"),
            &repo_path(relative),
        )
        .expect("re-parse");
        assert_eq!(parsed, again, "{relative} must round-trip");
    }
}

#[test]
fn the_fixture_declares_a_faster_whisper_and_a_kokoro_profile() {
    let d = BackendDescriptor::parse_with_path(&read(FIXTURE), Path::new(FIXTURE)).expect("parses");
    let ids: Vec<&str> = d.runner_profiles.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["faster_whisper", "kokoro"]);

    let stt = d.runner_profile("faster_whisper").expect("faster_whisper");
    assert_eq!(stt.interpreter, format!("{ROOT}/bin/python"));
    assert_eq!(stt.environment_root, ROOT);
    assert_eq!(
        stt.library_search_paths,
        [format!(
            "{ROOT}/lib/python3.12/site-packages/nvidia/cublas/lib"
        )]
    );
    assert_eq!(stt.compute_types, [ComputeType::Float16]);

    let tts = d.runner_profile("kokoro").expect("kokoro");
    assert!(tts.library_search_paths.is_empty());
    assert_eq!(tts.compute_types, [ComputeType::Float32]);
    assert!(d.runner_profile("smolvla").is_none());

    // The non-speech interpreter stays where it was.
    assert_eq!(
        d.python.and_then(|p| p.interpreter).as_deref(),
        Some("/usr/bin/python3")
    );
}

#[test]
fn the_shipped_descriptor_declares_no_runner_profile() {
    // No package installs a speech runner profile yet, so the shipped
    // descriptor must not declare one.
    let d = BackendDescriptor::parse_with_path(&read(SHIPPED), Path::new(SHIPPED)).expect("parses");
    assert!(d.runner_profiles.is_empty());
}

/// Malformed variants of the fixture that the schema and the reader both
/// refuse.
fn malformed_cases() -> Vec<(&'static str, Value)> {
    vec![
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
            "repeated library search path",
            with_profile_field(
                0,
                "library_search_paths",
                Some(json!([format!("{ROOT}/lib"), format!("{ROOT}/lib")])),
            ),
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
            "a compute type in map form",
            with_profile_field(0, "compute_types", Some(json!([{"float16": null}]))),
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
            "repeated package",
            with_profile_field(1, "packages", Some(json!(["a", "a"]))),
        ),
        (
            "id not lower_snake_case",
            with_profile_field(1, "id", Some(json!("Kokoro"))),
        ),
        (
            "missing interpreter",
            with_profile_field(0, "interpreter", None),
        ),
        ("missing packages", with_profile_field(1, "packages", None)),
        (
            "missing compute types",
            with_profile_field(1, "compute_types", None),
        ),
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
        ("an entry written as an array", {
            let mut doc = fixture();
            doc["runner_profiles"][0] = json!([
                "faster_whisper",
                format!("{ROOT}/bin/python"),
                ROOT,
                [],
                ["p"],
                ["float16"]
            ]);
            doc
        }),
    ]
}

#[test]
fn malformed_runner_profiles_are_refused_by_the_schema_and_the_reader() {
    let validator =
        jsonschema::JSONSchema::compile(&schema_document()).expect("schema compiles as Draft-07");
    let cases = malformed_cases();
    for (label, instance) in cases {
        assert!(
            !validator.is_valid(&instance),
            "{label}: the schema must refuse it"
        );
        assert!(
            !rust_accepts(&instance),
            "{label}: the reader must refuse it"
        );
    }
}

/// Variants the schema accepts and the reader refuses, each with the reason
/// the reader must give.
fn reader_only_cases() -> Vec<(&'static str, Value, &'static str)> {
    let outside_root = "must sit inside `environment_root`";
    let mut cases = vec![
        (
            "duplicate profile id",
            {
                let mut doc = fixture();
                doc["runner_profiles"][1]["id"] = json!("faster_whisper");
                doc
            },
            "is declared more than once",
        ),
        (
            "interpreter outside the environment root",
            with_profile_field(0, "interpreter", Some(json!("/usr/bin/python3"))),
            outside_root,
        ),
        (
            "interpreter equal to the environment root",
            with_profile_field(0, "interpreter", Some(json!(ROOT))),
            outside_root,
        ),
        (
            "interpreter escaping the root through `..`",
            with_profile_field(
                0,
                "interpreter",
                Some(json!(format!("{ROOT}/../../../bin/python3"))),
            ),
            "`interpreter` must be an absolute path",
        ),
        (
            "interpreter with a `.` component",
            with_profile_field(
                0,
                "interpreter",
                Some(json!(format!("{ROOT}/./bin/python"))),
            ),
            "`interpreter` must be an absolute path",
        ),
        (
            "environment root with a `.` component",
            with_profile_field(
                0,
                "environment_root",
                Some(json!("/usr/lib/tensorplate/./speech-runtime")),
            ),
            "`environment_root` must be an absolute path",
        ),
        (
            "environment root with a `..` component, interpreter to match",
            {
                let escaped = "/usr/lib/tensorplate/../tensorplate/speech-runtime";
                let mut doc = with_profile_field(0, "environment_root", Some(json!(escaped)));
                doc["runner_profiles"][0]["interpreter"] = json!(format!("{escaped}/bin/python"));
                doc
            },
            "`environment_root` must be an absolute path",
        ),
        (
            "blank-only package name",
            with_profile_field(1, "packages", Some(json!([" "]))),
            "must name at least one non-empty package",
        ),
    ];
    cases.extend(library_path_cases());
    cases
}

/// The reader-only cases for `library_search_paths`.
fn library_path_cases() -> Vec<(&'static str, Value, &'static str)> {
    let segment = "must be an absolute path with no `.` or `..` components";
    vec![
        (
            "library search path outside the environment root",
            with_profile_field(
                0,
                "library_search_paths",
                Some(json!(["/usr/lib/x86_64-linux-gnu"])),
            ),
            "must sit inside `environment_root`",
        ),
        (
            "library search path escaping the root through `..`",
            with_profile_field(
                0,
                "library_search_paths",
                Some(json!([format!(
                    "{ROOT}/../../../../usr/lib/x86_64-linux-gnu"
                )])),
            ),
            segment,
        ),
        (
            "library search path with a `.` component",
            with_profile_field(
                0,
                "library_search_paths",
                Some(json!([format!("{ROOT}/./lib")])),
            ),
            segment,
        ),
        (
            "library search path repeated with a trailing slash",
            with_profile_field(
                0,
                "library_search_paths",
                Some(json!([format!("{ROOT}/lib"), format!("{ROOT}/lib/")])),
            ),
            "`library_search_paths` repeats",
        ),
    ]
}

#[test]
fn rules_the_schema_cannot_state_are_enforced_by_the_reader() {
    // The schema accepts each of these; the reader refuses them, and for the
    // reason named. Pinning the schema's verdict keeps the list honest if
    // the schema grows a rule; pinning the reason keeps each case on the
    // rule it is named for, not on a neighbour that happens to refuse it.
    let validator =
        jsonschema::JSONSchema::compile(&schema_document()).expect("schema compiles as Draft-07");
    let cases = reader_only_cases();
    for (label, instance, reason) in cases {
        assert!(
            validator.is_valid(&instance),
            "{label}: the schema accepts it, so the reader is the only guard"
        );
        let refused =
            reader_error(&instance).unwrap_or_else(|| panic!("{label}: the reader must refuse it"));
        assert!(
            refused.contains(reason),
            "{label}: refused for another reason: {refused}"
        );
    }
}

#[test]
fn a_descriptor_with_an_empty_runner_profile_list_parses() {
    let mut doc = fixture();
    doc["runner_profiles"] = json!([]);
    let validator =
        jsonschema::JSONSchema::compile(&schema_document()).expect("schema compiles as Draft-07");
    assert!(validator.is_valid(&doc));
    assert!(rust_accepts(&doc));
}
