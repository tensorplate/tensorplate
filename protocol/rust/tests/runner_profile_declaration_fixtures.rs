// SPDX-License-Identifier: Apache-2.0
//
// Runner profile declarations: the files the speech runtime's profile
// packages install beside `backend.json`. The packaged declarations must
// validate against `definitions.runner_profile_declaration` of
// `protocol/schemas/backend_descriptor.json` and parse in the Rust mirror;
// `fixtures/runner_profile_declarations/cases.json` lists what each install
// combination leaves on disk, and the reader must make of each what its
// case says.

#![allow(clippy::expect_used, clippy::panic)]

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tensorplate_protocol::backend_descriptor::{
    BackendDescriptor, BackendDescriptorError, RunnerProfileDeclaration,
    RUNNER_PROFILE_DECLARATION_DIR,
};
use tensorplate_protocol::package_inventory::PackageInventory;

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
const BASE: &str = "packaging/backend-metadata/python_pytorch.json";
const MERGED: &str = "protocol/rust/tests/fixtures/backend_descriptor_runner_profiles.json";
const CASES: &str = "protocol/rust/tests/fixtures/runner_profile_declarations/cases.json";
const SECOND: &str =
    "protocol/rust/tests/fixtures/runner_profile_declarations/second_faster_whisper.json";
const RULE_CASES: &str = "protocol/rust/tests/fixtures/runner_profile_declarations/rule_cases.json";
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
        (
            // The members in declaration order: a derived reader would
            // take it for the object.
            "its members as an array",
            json!([
                read_json(STT)["$schema"],
                "0.1",
                "python_pytorch",
                read_json(STT)["runner_profile"]
            ]),
        ),
        ("missing schema_version", stt_with("/schema_version", None)),
        (
            "null schema_version",
            stt_with("/schema_version", Some(Value::Null)),
        ),
        (
            "another schema_version",
            stt_with("/schema_version", Some(json!("0.2"))),
        ),
        ("a null $schema", stt_with("/$schema", Some(Value::Null))),
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
            "a profile's members as an array",
            stt_with(
                "/runner_profile",
                Some(json!([
                    "faster_whisper",
                    "/usr/lib/tensorplate/speech-runtime/bin/python",
                    "/usr/lib/tensorplate/speech-runtime",
                    [],
                    ["tensorplate-speech-runtime-ct2"],
                    ["float16"]
                ])),
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

/// A package database with a fixed content that records what it is asked.
struct Installed {
    packages: BTreeSet<String>,
    asked: RefCell<Vec<BTreeSet<String>>>,
}

impl Installed {
    fn new<'a>(packages: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            packages: packages.into_iter().map(str::to_owned).collect(),
            asked: RefCell::new(Vec::new()),
        }
    }

    fn of_case(case: &Value) -> Self {
        let packages = case["installed_packages"].as_array().expect("packages");
        Self::new(packages.iter().map(|p| p.as_str().expect("package")))
    }
}

impl PackageInventory for Installed {
    fn installed(&self, names: &BTreeSet<&str>) -> Result<BTreeSet<String>, String> {
        let names: BTreeSet<String> = names.iter().map(ToString::to_string).collect();
        self.asked.borrow_mut().push(names.clone());
        Ok(&names & &self.packages)
    }
}

/// A package database that cannot be asked.
struct Unreadable;

impl PackageInventory for Unreadable {
    fn installed(&self, _: &BTreeSet<&str>) -> Result<BTreeSet<String>, String> {
        Err("the package database is locked".into())
    }
}

/// An installed backend: `descriptor` as `python_pytorch/backend.json`, and
/// `declarations` (file name to repository file) in the directory beside
/// it, which is absent when `declarations` is not an object.
fn install(descriptor: &str, declarations: &Value) -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().expect("tempdir");
    let backend = root.path().join("python_pytorch");
    std::fs::create_dir(&backend).expect("backend directory");
    let path = backend.join("backend.json");
    std::fs::copy(repo_path(descriptor), &path).expect("descriptor");
    if let Some(files) = declarations.as_object() {
        let directory = backend.join(RUNNER_PROFILE_DECLARATION_DIR);
        std::fs::create_dir(&directory).expect("declaration directory");
        for (name, source) in files {
            let source = repo_path(source.as_str().expect("source path"));
            std::fs::copy(source, directory.join(name)).expect("declaration");
        }
    }
    (root, path)
}

fn variant(err: &BackendDescriptorError) -> &'static str {
    match err {
        BackendDescriptorError::Missing { .. } => "Missing",
        BackendDescriptorError::Io { .. } => "Io",
        BackendDescriptorError::Malformed { .. } => "Malformed",
        BackendDescriptorError::UnsupportedSchemaVersion { .. } => "UnsupportedSchemaVersion",
        BackendDescriptorError::Invalid { .. } => "Invalid",
        BackendDescriptorError::DuplicateRunnerProfile { .. } => "DuplicateRunnerProfile",
        BackendDescriptorError::PackageNotInstalled { .. } => "PackageNotInstalled",
        BackendDescriptorError::PackageInventoryUnavailable { .. } => "PackageInventoryUnavailable",
    }
}

fn ids(descriptor: &BackendDescriptor) -> Vec<&str> {
    descriptor
        .runner_profiles
        .iter()
        .map(|p| p.id.as_str())
        .collect()
}

fn parse_declaration(instance: &Value) -> Result<RunnerProfileDeclaration, BackendDescriptorError> {
    RunnerProfileDeclaration::parse_with_path(&instance.to_string(), Path::new(STT))
}

#[test]
fn every_install_combination_reads_as_its_case_says() {
    let cases = read_json(CASES);
    let mut seen = BTreeSet::new();
    for case in cases["cases"].as_array().expect("cases") {
        let name = case["name"].as_str().expect("name");
        seen.insert(name);
        let (_root, path) = install(BASE, &case["declarations"]);
        let inventory = Installed::of_case(case);
        let read = BackendDescriptor::read_with_inventory(&path, &inventory);
        match (read, case.get("refusal")) {
            (Ok(descriptor), None) => {
                let expected: Vec<&str> = case["runner_profiles"]
                    .as_array()
                    .expect("runner_profiles")
                    .iter()
                    .map(|id| id.as_str().expect("id"))
                    .collect();
                assert_eq!(ids(&descriptor), expected, "{name}");
            }
            (Err(err), Some(refusal)) => {
                assert_eq!(variant(&err), refusal["variant"], "{name}: {err}");
                for needle in refusal["names"].as_array().expect("names") {
                    let needle = needle.as_str().expect("needle");
                    assert!(
                        err.to_string().contains(needle),
                        "{name}: `{err}` lacks `{needle}`"
                    );
                }
            }
            (Ok(descriptor), Some(refusal)) => {
                panic!("{name}: read {:?}, expected {refusal}", ids(&descriptor))
            }
            (Err(err), None) => panic!("{name}: refused: {err}"),
        }
    }
    for required in [
        "none",
        "stt_only",
        "tts_only",
        "both",
        "duplicate_profile_id",
        "a_package_of_the_profile_is_not_installed",
        "a_package_of_the_second_profile_is_not_installed",
        "malformed_file",
    ] {
        assert!(seen.contains(required), "cases.json lost `{required}`");
    }
}

#[test]
fn a_descriptor_with_no_declaration_reads_as_it_parses_and_asks_nothing() {
    let parsed = BackendDescriptor::parse_with_path(&read(BASE), &repo_path(BASE)).expect("parses");
    for declarations in [Value::Null, json!({})] {
        let (_root, path) = install(BASE, &declarations);
        let inventory = Installed::new([]);
        let read = BackendDescriptor::read_with_inventory(&path, &inventory).expect("reads");
        assert_eq!(read, parsed);
        assert!(inventory.asked.borrow().is_empty());
    }
    // The dpkg-backed reader therefore needs no dpkg on such a host.
    let (_root, path) = install(BASE, &Value::Null);
    assert_eq!(BackendDescriptor::read_from(&path).expect("reads"), parsed);
}

#[test]
fn both_packages_give_the_merged_fixtures_view_and_one_question() {
    let (_root, path) = install(
        BASE,
        &json!({"faster_whisper.json": STT, "kokoro.json": TTS}),
    );
    let every_package: BTreeSet<String> = [STT, TTS]
        .iter()
        .flat_map(|relative| {
            let declaration = parse_declaration(&read_json(relative)).expect("parses");
            declaration.runner_profile.packages
        })
        .collect();
    let inventory = Installed::new(every_package.iter().map(String::as_str));
    let installed = BackendDescriptor::read_with_inventory(&path, &inventory).expect("reads");
    let merged =
        BackendDescriptor::parse_with_path(&read(MERGED), &repo_path(MERGED)).expect("parses");
    assert_eq!(installed.runner_profiles, merged.runner_profiles);
    assert_eq!(*inventory.asked.borrow(), [every_package]);
}

#[test]
fn declarations_merge_in_file_name_order() {
    // Eight files written in an order that is not their names' order; a
    // directory listing returns them in neither.
    let (_root, path) = install(BASE, &json!({}));
    let directory = path.with_file_name(RUNNER_PROFILE_DECLARATION_DIR);
    for n in [5, 2, 7, 0, 3, 6, 1, 4] {
        let declaration = stt_with("/runner_profile/id", Some(json!(format!("profile_{n}"))));
        std::fs::write(directory.join(format!("{n}.json")), declaration.to_string())
            .expect("write");
    }
    let read = BackendDescriptor::read_with_inventory(&path, &Unconditional).expect("reads");
    let expected: Vec<String> = (0..8).map(|n| format!("profile_{n}")).collect();
    assert_eq!(ids(&read), expected);
}

/// A package database in which every package is installed.
struct Unconditional;

impl PackageInventory for Unconditional {
    fn installed(&self, names: &BTreeSet<&str>) -> Result<BTreeSet<String>, String> {
        Ok(names.iter().map(ToString::to_string).collect())
    }
}

/// The sidecar launcher has a reader of its own for these files and runs
/// the same cases: what one reader accepts, the other must.
#[test]
fn every_rule_case_is_accepted_or_refused_as_it_says() {
    let cases = read_json(RULE_CASES);
    let cases = cases["cases"].as_array().expect("cases");
    let mut accepted = 0;
    for case in cases {
        let name = case["name"].as_str().expect("name");
        let (_root, path) = install(BASE, &json!({}));
        let file = path
            .with_file_name(RUNNER_PROFILE_DECLARATION_DIR)
            .join("case.json");
        let text = match case.get("text") {
            Some(text) => text.as_str().expect("text").to_owned(),
            None => case["declaration"].to_string(),
        };
        std::fs::write(&file, text).expect("declaration");

        let read = BackendDescriptor::read_with_inventory(&path, &Unconditional);

        let expected = case["accepted"].as_bool().expect("accepted");
        let outcome = read.as_ref().map(ids).map_err(ToString::to_string);
        assert_eq!(read.is_ok(), expected, "{name}: {outcome:?}");
        accepted += usize::from(expected);
    }
    // The launcher's test pins the same two counts.
    assert_eq!((accepted, cases.len() - accepted), (10, 55));
}

#[test]
fn a_profile_the_descriptor_itself_lists_is_held_to_the_same_rules() {
    // Declared twice: by the descriptor and by a declaration beside it.
    let (_root, path) = install(MERGED, &json!({"faster_whisper.json": STT}));
    let err = BackendDescriptor::read_with_inventory(&path, &Unconditional).expect_err("twice");
    match &err {
        BackendDescriptorError::DuplicateRunnerProfile { id, first, second } => {
            assert_eq!(id, "faster_whisper");
            assert_eq!(first, &path.display().to_string());
            assert!(second.ends_with("runner_profiles.d/faster_whisper.json"));
        }
        other => panic!("expected a duplicate, got {other}"),
    }

    // Its packages are checked as a declaration's are.
    let (_root, path) = install(MERGED, &Value::Null);
    let inventory = Installed::new(["tensorplate-speech-runtime-base"]);
    let err = BackendDescriptor::read_with_inventory(&path, &inventory).expect_err("packages");
    match &err {
        BackendDescriptorError::PackageNotInstalled {
            path: at,
            profile,
            package,
        } => {
            assert_eq!(at, &path.display().to_string());
            assert_eq!(profile, "faster_whisper");
            assert_eq!(package, "tensorplate-speech-runtime-ct2");
        }
        other => panic!("expected a missing package, got {other}"),
    }
}

#[test]
fn a_package_database_that_cannot_be_asked_refuses_the_descriptor() {
    let (_root, path) = install(BASE, &json!({"kokoro.json": TTS}));
    let err = BackendDescriptor::read_with_inventory(&path, &Unreadable).expect_err("unreadable");
    assert_eq!(variant(&err), "PackageInventoryUnavailable", "{err}");
    assert!(err.to_string().contains("the package database is locked"));
}

#[test]
fn a_declaration_for_another_backend_is_refused() {
    let (_root, path) = install(BASE, &json!({"faster_whisper.json": STT}));
    let file = path
        .with_file_name(RUNNER_PROFILE_DECLARATION_DIR)
        .join("faster_whisper.json");
    let other = stt_with("/backend_name", Some(json!("tensorrt")));
    assert!(
        declaration_validator().is_valid(&other),
        "a rule across two files"
    );
    std::fs::write(file, other.to_string()).expect("write");
    let err = BackendDescriptor::read_with_inventory(&path, &Unconditional).expect_err("backend");
    assert_eq!(variant(&err), "Invalid", "{err}");
    assert!(err.to_string().contains("`tensorrt`") && err.to_string().contains("`python_pytorch`"));
}

#[test]
fn an_entry_that_is_not_a_readable_file_is_refused_not_skipped() {
    let (_root, path) = install(BASE, &json!({"faster_whisper.json": STT}));
    let directory = path.with_file_name(RUNNER_PROFILE_DECLARATION_DIR);
    std::fs::create_dir(directory.join("kokoro.json")).expect("a directory named as a declaration");
    let err = BackendDescriptor::read_with_inventory(&path, &Unconditional).expect_err("directory");
    assert_eq!(variant(&err), "Io", "{err}");
    assert!(err.to_string().contains("runner_profiles.d/kokoro.json"));

    // Listed and then gone: not the "backend is not installed" answer an
    // absent descriptor gets.
    let (_root, path) = install(BASE, &json!({"faster_whisper.json": STT}));
    let directory = path.with_file_name(RUNNER_PROFILE_DECLARATION_DIR);
    std::os::unix::fs::symlink(directory.join("removed"), directory.join("kokoro.json"))
        .expect("a link to nothing");
    let err = BackendDescriptor::read_with_inventory(&path, &Unconditional).expect_err("dangling");
    assert_eq!(variant(&err), "Io", "{err}");
    assert!(err.to_string().contains("runner_profiles.d/kokoro.json"));
}

#[test]
fn a_declaration_directory_that_cannot_be_listed_is_refused_not_read_as_empty() {
    let (_root, path) = install(BASE, &Value::Null);
    let directory = path.with_file_name(RUNNER_PROFILE_DECLARATION_DIR);
    std::fs::write(directory, "").expect("a file where the directory belongs");
    let err = BackendDescriptor::read_with_inventory(&path, &Unconditional).expect_err("a file");
    assert_eq!(variant(&err), "Io", "{err}");
    assert!(err.to_string().contains("runner_profiles.d`"), "{err}");
}

#[test]
fn the_descriptors_own_profiles_come_before_the_declared_ones() {
    // The descriptor lists `kokoro`; `faster_whisper` is declared beside it
    // and would sort first by name.
    let root = tempfile::tempdir().expect("tempdir");
    let backend = root.path().join("python_pytorch");
    let directory = backend.join(RUNNER_PROFILE_DECLARATION_DIR);
    std::fs::create_dir_all(&directory).expect("directories");
    let mut descriptor = read_json(MERGED);
    descriptor["runner_profiles"]
        .as_array_mut()
        .expect("profiles")
        .retain(|profile| profile["id"] == "kokoro");
    let path = backend.join("backend.json");
    std::fs::write(&path, descriptor.to_string()).expect("descriptor");
    std::fs::copy(repo_path(STT), directory.join("faster_whisper.json")).expect("declaration");
    let read = BackendDescriptor::read_with_inventory(&path, &Unconditional).expect("reads");
    assert_eq!(ids(&read), ["kokoro", "faster_whisper"]);
}

#[test]
fn the_reader_refuses_what_the_schema_refuses() {
    for relative in [STT, TTS, SECOND] {
        parse_declaration(&read_json(relative)).unwrap_or_else(|e| panic!("{relative}: {e}"));
    }
    for (name, instance) in schema_refused_cases() {
        let Err(err) = parse_declaration(&instance) else {
            panic!("the reader accepted {name}");
        };
        let expected = match name {
            "another schema_version" => "UnsupportedSchemaVersion",
            "empty backend_name" | "a relative interpreter" | "no package" => "Invalid",
            _ => "Malformed",
        };
        assert_eq!(variant(&err), expected, "{name}: {err}");
    }
}

#[test]
fn the_reader_holds_a_declared_profile_to_the_descriptors_rules() {
    // The schema cannot state these; the same check serves `runner_profiles`.
    let validator = declaration_validator();
    for (name, instance, reason) in [
        (
            "an interpreter outside the environment",
            stt_with(
                "/runner_profile/interpreter",
                Some(json!("/usr/bin/python3")),
            ),
            "`interpreter` must sit inside `environment_root`",
        ),
        (
            "a parent segment",
            stt_with(
                "/runner_profile/environment_root",
                Some(json!(
                    "/usr/lib/tensorplate/speech-runtime/../speech-runtime"
                )),
            ),
            "no `.` or `..` components",
        ),
        (
            "a blank package",
            stt_with("/runner_profile/packages", Some(json!([" "]))),
            "at least one non-empty package",
        ),
    ] {
        assert!(validator.is_valid(&instance), "the schema refused {name}");
        let err = parse_declaration(&instance).expect_err(name);
        assert_eq!(variant(&err), "Invalid", "{name}: {err}");
        assert!(err.to_string().contains(reason), "{name}: {err}");
    }
}
