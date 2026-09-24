// SPDX-License-Identifier: Apache-2.0
//
// Provisioning manifest fixtures. The committed fixture and the manifest
// `tensorplate-cli` ships must validate against
// `protocol/schemas/provisioning_manifest.json` and parse in the Rust
// reader. Every malformed variant the schema can express must be refused by
// both; the rules only the reader can state are listed separately, each
// refused for the reason it is named for, with the schema's acceptance
// pinned.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use serde_json::{json, Value};
use tensorplate_protocol::provisioning_manifest::ProvisioningManifest;

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn read(relative: &str) -> String {
    let path = repo_path(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

const FIXTURE: &str = "protocol/rust/tests/fixtures/provisioning_manifest.json";
const SHIPPED: &str = "packaging/provisioning/manifest.json";
const SOURCE: &str = "test/models/bundles/v0_1/smolvla_python_pytorch";

fn validator() -> jsonschema::JSONSchema {
    let schema: Value = serde_json::from_str(&read("protocol/schemas/provisioning_manifest.json"))
        .expect("schema parses");
    jsonschema::JSONSchema::compile(&schema).expect("schema compiles as Draft-07")
}

fn fixture() -> Value {
    serde_json::from_str(&read(FIXTURE)).expect("fixture parses as JSON")
}

/// The fixture with its first file's `field` replaced.
fn with_file_field(field: &str, value: Value) -> Value {
    let mut doc = fixture();
    doc["bundles"][0]["files"][0][field] = value;
    doc
}

fn reader_error(doc: &Value) -> Option<String> {
    ProvisioningManifest::parse(&serde_json::to_string(doc).expect("serialize"))
        .err()
        .map(|err| err.to_string())
}

#[test]
fn committed_manifests_validate_and_parse() {
    let validator = validator();
    for relative in [FIXTURE, SHIPPED] {
        let doc: Value = serde_json::from_str(&read(relative)).expect("parses as JSON");
        assert!(validator.is_valid(&doc), "{relative} must validate");
        let parsed = ProvisioningManifest::parse(&read(relative))
            .unwrap_or_else(|e| panic!("{relative} must parse: {e}"));
        let again =
            ProvisioningManifest::parse(&serde_json::to_string(&parsed).expect("serialize"))
                .expect("re-parse");
        assert_eq!(parsed, again, "{relative} must round-trip");
    }
    // Nothing is provisionable until the model bundles are pinned.
    assert!(ProvisioningManifest::parse(&read(SHIPPED))
        .expect("shipped")
        .bundles
        .is_empty());
}

#[test]
fn the_fixture_describes_the_committed_source_bundle_file_for_file() {
    use sha2::{Digest, Sha256};

    let manifest = ProvisioningManifest::parse(&read(FIXTURE)).expect("fixture");
    let bundle = manifest
        .bundle("smolvla-fixture")
        .expect("the fixture bundle");
    let root = repo_path(SOURCE);
    let mut on_disk = Vec::new();
    let mut pending = vec![root.clone()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                on_disk.push(
                    path.strip_prefix(&root)
                        .expect("inside")
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    on_disk.sort();
    let mut listed: Vec<String> = bundle.files.iter().map(|f| f.path.clone()).collect();
    listed.sort();
    assert_eq!(
        listed, on_disk,
        "the manifest lists exactly the source's files"
    );
    for file in &bundle.files {
        let bytes = std::fs::read(root.join(&file.path)).expect("source file");
        assert_eq!(bytes.len() as u64, file.size, "{}", file.path);
        assert_eq!(
            format!("{:x}", Sha256::digest(&bytes)),
            file.sha256,
            "{}",
            file.path
        );
    }
}

#[test]
fn malformed_manifests_are_refused_by_the_schema_and_the_reader() {
    let validator = validator();
    let mut unknown_top = fixture();
    unknown_top["signed_by"] = json!("someone");
    let mut unknown_file = fixture();
    unknown_file["bundles"][0]["files"][0]["url"] = json!("https://example.invalid/x");
    let mut no_files = fixture();
    no_files["bundles"][0]["files"] = json!([]);
    let mut bad_name = fixture();
    bad_name["bundles"][0]["name"] = json!("SmolVLA");
    let cases: Vec<(&str, Value)> = vec![
        ("another schema version", {
            let mut doc = fixture();
            doc["schema_version"] = json!("0.2");
            doc
        }),
        ("an unknown top-level field", unknown_top),
        ("an unknown file field", unknown_file),
        ("a bundle with no files", no_files),
        ("an uppercase bundle name", bad_name),
        (
            "an absolute path",
            with_file_field("path", json!("/etc/passwd")),
        ),
        (
            "a `..` segment",
            with_file_field("path", json!("assets/../../x")),
        ),
        (
            "a `.` segment",
            with_file_field("path", json!("./manifest.json")),
        ),
        (
            "an empty segment",
            with_file_field("path", json!("assets//x")),
        ),
        (
            "a hidden segment",
            with_file_field("path", json!(".cache/x")),
        ),
        ("a backslash", with_file_field("path", json!("assets\\x"))),
        (
            "an uppercase digest",
            with_file_field("sha256", json!("A".repeat(64))),
        ),
        ("a short digest", with_file_field("sha256", json!("abc"))),
        ("a negative size", with_file_field("size", json!(-1))),
        ("a missing size", {
            let mut doc = fixture();
            doc["bundles"][0]["files"][0]
                .as_object_mut()
                .expect("object")
                .remove("size");
            doc
        }),
    ];
    for (label, doc) in cases {
        assert!(
            !validator.is_valid(&doc),
            "{label}: the schema must refuse it"
        );
        assert!(
            reader_error(&doc).is_some(),
            "{label}: the reader must refuse it"
        );
    }
    // An empty list also lacks `manifest.json`; the reader must say which.
    let mut no_files = fixture();
    no_files["bundles"][0]["files"] = json!([]);
    let refused = reader_error(&no_files).expect("refused");
    assert!(refused.contains("lists no files"), "{refused}");
}

#[test]
fn rules_the_schema_cannot_state_are_enforced_by_the_reader() {
    let validator = validator();
    let mut repeated_bundle = fixture();
    let copy = repeated_bundle["bundles"][0].clone();
    repeated_bundle["bundles"]
        .as_array_mut()
        .expect("bundles")
        .push(copy);
    let mut repeated_path = fixture();
    let copy = repeated_path["bundles"][0]["files"][1].clone();
    repeated_path["bundles"][0]["files"]
        .as_array_mut()
        .expect("files")
        .push(copy);
    let mut file_and_directory = fixture();
    file_and_directory["bundles"][0]["files"]
        .as_array_mut()
        .expect("files")
        .push(json!({"path": "assets", "sha256": "0".repeat(64), "size": 0}));
    let mut no_bundle_manifest = fixture();
    no_bundle_manifest["bundles"][0]["files"]
        .as_array_mut()
        .expect("files")
        .retain(|file| file["path"] != "manifest.json");
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "a repeated bundle name",
            repeated_bundle,
            "is listed more than once",
        ),
        ("a repeated path", repeated_path, "more than once"),
        (
            "a path that is also a directory",
            file_and_directory,
            "both as a file and as a directory",
        ),
        (
            "no bundle manifest",
            no_bundle_manifest,
            "does not list `manifest.json`",
        ),
    ];
    for (label, doc, reason) in cases {
        assert!(
            validator.is_valid(&doc),
            "{label}: the schema accepts it, so the reader is the only guard"
        );
        let refused =
            reader_error(&doc).unwrap_or_else(|| panic!("{label}: the reader must refuse it"));
        assert!(
            refused.contains(reason),
            "{label}: refused for another reason: {refused}"
        );
    }
}
