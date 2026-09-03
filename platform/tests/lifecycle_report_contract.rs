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

/// Run the converter over a harness-shaped stage log.
fn convert(rows: &[(&str, &str)], maps: &[&str]) -> (Value, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let tsv = dir.path().join("stages.tsv");
    let mut body = String::from("stage\tstatus\tstarted_at\tfinished_at\tlog\n");
    for (stage, status) in rows {
        body.push_str(&format!(
            "{stage}\t{status}\t2026-09-03T03:00:00Z\t2026-09-03T03:01:00Z\t{stage}.log\n"
        ));
    }
    std::fs::write(&tsv, body).expect("write stage log");
    let out = dir.path().join("lifecycle-report.json");
    let status = Command::new(repo_root().join("tools/validation/lifecycle-report-from-stages.sh"))
        .arg(&tsv)
        .arg("macos26-m1pro-16gb")
        .arg("contract-test")
        .arg(&out)
        .args(maps)
        .status()
        .expect("run converter");
    assert!(status.success(), "converter must succeed on a valid log");
    let doc: Value =
        serde_json::from_str(&std::fs::read_to_string(out).expect("read report")).expect("parses");
    (doc, dir)
}

#[test]
fn a_converted_harness_log_validates_and_carries_all_eight_stages() {
    // A harness that ran only some of the canonical stages still produces
    // a complete report: the release gate has to see the absent ones to
    // report them missing, and a shorter report would read as a shorter
    // run rather than an incomplete one.
    let (doc, _dir) = convert(
        &[("clean-install", "pass"), ("deploy-smoke", "pass")],
        &["clean-install=install", "deploy-smoke=deploy-smoke"],
    );
    let errors = validation_errors(&compiled_schema(), &doc);
    assert!(
        errors.is_empty(),
        "converted report must validate: {errors:?}"
    );
    assert_eq!(
        doc["stages"].as_array().expect("stages").len(),
        8,
        "every canonical stage appears, run or not"
    );
    assert_eq!(doc["outcome"], "pass");
}

#[test]
fn a_converted_report_distinguishes_an_unmapped_stage_from_an_unrun_one() {
    // Two different problems with two different fixes: nobody taught the
    // converter which harness stage covers this, versus the harness knows
    // and did not get there.
    let (doc, _dir) = convert(
        &[("clean-install", "pass")],
        &["clean-install=install", "launchd-restart=restart"],
    );
    let detail = |stage: &str| -> String {
        doc["stages"]
            .as_array()
            .expect("stages")
            .iter()
            .find(|s| s["stage"] == stage)
            .and_then(|s| s["detail"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    assert!(
        detail("restart").contains("did not run"),
        "a mapped stage absent from the log is an unrun stage: {}",
        detail("restart")
    );
    assert!(
        detail("offline").contains("no harness stage mapped"),
        "an unmapped canonical stage says so: {}",
        detail("offline")
    );
}

#[test]
fn a_failed_harness_stage_survives_conversion() {
    // The property the gate depends on: conversion must not launder a
    // failure into a pass or an omission.
    let (doc, _dir) = convert(
        &[("clean-install", "pass"), ("deploy-smoke", "fail")],
        &["clean-install=install", "deploy-smoke=deploy-smoke"],
    );
    assert_eq!(doc["outcome"], "fail");
    let smoke = doc["stages"]
        .as_array()
        .expect("stages")
        .iter()
        .find(|s| s["stage"] == "deploy-smoke")
        .expect("deploy-smoke present");
    assert_eq!(smoke["status"], "fail");
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
