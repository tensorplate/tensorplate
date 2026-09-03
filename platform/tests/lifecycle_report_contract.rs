// SPDX-License-Identifier: Apache-2.0
//
// The lifecycle report is the release gate's input, so its schema is a
// contract between a shell harness and the Rust that will read it.
//
// A schema nothing produces drifts from the producer silently. These run
// the real harness and validate what it actually writes, rather than a
// hand-written sample that only proves the schema parses.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn schema() -> Value {
    let path = repo_root().join("config/schemas/lifecycle_report.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("read schema"))
        .expect("schema parses")
}

/// The compiled schema. Leaked so it can borrow for the test's lifetime,
/// the same way the config-schema contract test does it.
fn compiled_schema() -> jsonschema::JSONSchema {
    let document: &'static Value = Box::leak(Box::new(schema()));
    jsonschema::JSONSchema::compile(document).expect("the schema itself is valid")
}

/// Validate, collecting messages before the borrow ends.
fn validation_errors(compiled: &jsonschema::JSONSchema, doc: &Value) -> Vec<String> {
    match compiled.validate(doc) {
        Ok(()) => Vec::new(),
        Err(errors) => errors
            .map(|e| format!("{e} at {}", e.instance_path))
            .collect(),
    }
}

/// Drive the shell harness and return the report it wrote.
fn run_harness(script: &str) -> (Value, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let harness = repo_root().join("tools/validation/lifecycle-stages.sh");
    let body = format!(
        "source '{}'\nlifecycle_begin ubuntu2404-x86-l4-g2s8 '{}' contract-test\ntrap 'lifecycle_abort $?' EXIT\n{}\n",
        harness.display(),
        dir.path().display(),
        script
    );
    // Not `.status()` on a shell that may exit non-zero by design: the
    // failing-run case is one of the things under test.
    let _ = Command::new("bash")
        .arg("-c")
        .arg(&body)
        .output()
        .expect("run harness");
    let report = dir.path().join("lifecycle-report.json");
    let doc: Value = serde_json::from_str(
        &std::fs::read_to_string(report).expect("the harness must write a report"),
    )
    .expect("report parses");
    (doc, dir)
}

#[test]
fn a_completed_run_validates_against_the_schema() {
    let (doc, _dir) = run_harness("lifecycle_stage install true\nlifecycle_finish");
    let errors = validation_errors(&compiled_schema(), &doc);
    assert!(
        errors.is_empty(),
        "harness output does not satisfy its own schema: {errors:?}"
    );
}

#[test]
fn an_aborted_run_still_validates_and_names_the_failed_stage() {
    // The case the release gate depends on: a run that died mid-way must
    // leave a well-formed report saying where, not a truncated file.
    let (doc, _dir) = run_harness(
        "lifecycle_stage install true\nlifecycle_stage deploy-smoke false\nlifecycle_finish",
    );
    let errors = validation_errors(&compiled_schema(), &doc);
    assert!(
        errors.is_empty(),
        "an aborted run must still produce a schema-valid report: {errors:?}"
    );
    assert_eq!(doc["outcome"], "fail");
    let failed: Vec<&str> = doc["stages"]
        .as_array()
        .expect("stages")
        .iter()
        .filter(|s| s["status"] == "fail")
        .map(|s| s["stage"].as_str().expect("stage name"))
        .collect();
    assert_eq!(failed, vec!["deploy-smoke"]);
}

#[test]
fn the_schema_and_the_harness_agree_on_the_eight_stage_names() {
    // Two lists of stage names, in two languages, that must not drift.
    let schema = schema();
    let schema_stages: Vec<String> = schema["properties"]["stages"]["items"]["properties"]["stage"]
        ["enum"]
        .as_array()
        .expect("stage enum")
        .iter()
        .map(|v| v.as_str().expect("stage name").to_string())
        .collect();

    let harness = std::fs::read_to_string(repo_root().join("tools/validation/lifecycle-stages.sh"))
        .expect("read harness");
    let block = harness
        .split_once("LIFECYCLE_STAGES=(")
        .expect("the harness declares its stage list")
        .1
        .split_once(')')
        .expect("stage list terminates")
        .0;
    let harness_stages: Vec<String> = block
        .split_whitespace()
        .map(std::string::ToString::to_string)
        .collect();

    assert_eq!(
        harness_stages, schema_stages,
        "the harness and the schema must name the same eight stages, in the same order"
    );
    assert_eq!(schema_stages.len(), 8);
}
