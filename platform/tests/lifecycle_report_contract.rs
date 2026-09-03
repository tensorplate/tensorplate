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
        "source '{}'\nlifecycle_begin ubuntu2404-x86-l4-g2s8 '{}' 0.2.1 contract-test\ntrap 'lifecycle_abort $?' EXIT\n{}\n",
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
        .arg("0.2.1")
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
    // Two mapped stages and six the harness never ran is not a validated
    // row. Reporting that as `pass` was how six skips could authorize a
    // release; `incomplete` says what actually happened.
    assert_eq!(doc["outcome"], "incomplete");
}

#[test]
fn a_converted_report_names_the_version_it_exercised() {
    // Evidence that does not say what it tested authorizes every later
    // tag, because nothing distinguishes it from a run against them.
    let (doc, _dir) = convert(&[("clean-install", "pass")], &["clean-install=install"]);
    assert_eq!(doc["subject"]["tested_version"], "0.2.1");
}

#[test]
fn conversion_reports_the_weakest_result_regardless_of_log_order() {
    // Several harness stages may cover one canonical stage. The rule is
    // that the worst of them wins -- and it must not depend on the order
    // they appear in the log. The previous rule replaced only a `pass`,
    // so a `skipped` seen first absorbed a later `fail` and the same two
    // stages produced `pass` or `fail` depending on their order.
    let maps = &["early=install", "late=install"];
    let (skip_first, _a) = convert(&[("early", "skipped"), ("late", "fail")], maps);
    let (fail_first, _b) = convert(&[("early", "fail"), ("late", "skipped")], maps);

    let install = |doc: &Value| -> String {
        doc["stages"]
            .as_array()
            .expect("stages")
            .iter()
            .find(|s| s["stage"] == "install")
            .and_then(|s| s["status"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(
        install(&skip_first),
        "fail",
        "a fail after a skip must survive"
    );
    assert_eq!(
        install(&fail_first),
        "fail",
        "a fail before a skip must survive"
    );
    assert_eq!(skip_first["outcome"], fail_first["outcome"]);
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

/// The canonical stage list as one of the shell tools declares it.
///
/// Each tool spells the list out for itself, because a shell script that
/// sourced a shared definition could not be run standalone on a machine
/// with only the harness copied onto it. Independent copies are the cost,
/// so the drift check has to cover every one of them: a stage added to
/// the schema but not the checker would be required by the contract and
/// unenforced by the gate.
fn declared_stages(relative: &str, marker: &str, terminator: char) -> Vec<String> {
    let body = std::fs::read_to_string(repo_root().join(relative))
        .unwrap_or_else(|e| panic!("read {relative}: {e}"));
    let block = body
        .split_once(marker)
        .unwrap_or_else(|| panic!("{relative} declares its stage list with `{marker}`"))
        .1
        .split_once(terminator)
        .unwrap_or_else(|| panic!("{relative} stage list terminates"))
        .0;
    block
        .split(|c: char| c.is_whitespace() || c == ',' || c == '"' || c == '\'')
        .filter(|token| !token.is_empty())
        .map(std::string::ToString::to_string)
        .collect()
}

#[test]
fn every_canonical_stage_list_agrees_with_the_schema() {
    // Four lists of stage names, in three languages, that must not drift:
    // the schema states the contract, the runner produces reports against
    // it, the converter derives them, and the checker gates a release on
    // them. A release is only as strict as the shortest of these.
    let schema = schema();
    let schema_stages: Vec<String> = schema["properties"]["stages"]["items"]["properties"]["stage"]
        ["enum"]
        .as_array()
        .expect("stage enum")
        .iter()
        .map(|v| v.as_str().expect("stage name").to_string())
        .collect();
    assert_eq!(schema_stages.len(), 8, "the canonical set is eight stages");

    let lists = [
        (
            "runner",
            declared_stages(
                "tools/validation/lifecycle-stages.sh",
                "LIFECYCLE_STAGES=(",
                ')',
            ),
        ),
        (
            "converter",
            declared_stages(
                "tools/validation/lifecycle-report-from-stages.sh",
                "CANONICAL = [",
                ']',
            ),
        ),
        (
            "checker",
            declared_stages(
                "tools/release/check-evidence-bundles.sh",
                "CANONICAL = [",
                ']',
            ),
        ),
    ];
    for (name, stages) in &lists {
        assert_eq!(
            stages, &schema_stages,
            "the {name} and the schema must name the same eight stages, in the same order"
        );
    }
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

/// Stage mappings the runbooks document, as `source=canonical` pairs.
fn documented_mappings() -> Vec<(String, String)> {
    let doc = std::fs::read_to_string(repo_root().join("docs/validation/physical-row-runbooks.md"))
        .expect("read runbooks");
    doc.split_whitespace()
        .filter(|token| token.contains('=') && !token.starts_with("--"))
        .filter_map(|token| token.split_once('='))
        .filter(|(source, target)| {
            !source.is_empty()
                && !target.is_empty()
                && source
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && target
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
        .map(|(source, target)| (source.to_string(), target.to_string()))
        .collect()
}

#[test]
fn the_runbooks_map_only_stages_that_exist() {
    // The runbooks' mappings are assertions about the harnesses, and a
    // mapping naming a stage no harness runs claims evidence for a stage
    // that never happened -- which the runbook, being prose about code
    // that only runs on hardware, cannot otherwise reveal.
    //
    // What this catches is a mapping whose names are wrong: a renamed or
    // misspelled harness stage, or a target that is not canonical. What
    // it cannot catch is a mapping whose names are all real but whose
    // claim is false -- `host-facts=status-logs` named two real stages
    // and still asserted that collecting inventory proved status and log
    // behaviour. That one stays a human judgement, which is why the
    // runbook states the coverage gaps explicitly instead of filling
    // them with the nearest plausible stage.
    let mappings = documented_mappings();
    assert!(
        mappings.len() >= 10,
        "expected the runbooks to document mappings, found {}",
        mappings.len()
    );

    let canonical: Vec<String> = schema()["properties"]["stages"]["items"]["properties"]["stage"]
        ["enum"]
        .as_array()
        .expect("stage enum")
        .iter()
        .map(|v| v.as_str().expect("stage name").to_string())
        .collect();

    // Whole names, not substrings: `contains("run_stage clean-instal")`
    // matches the line declaring `clean-install`, so a truncated or
    // misspelled stage name would pass a substring check.
    let known = harness_stage_names();
    for (source, target) in &mappings {
        assert!(
            canonical.contains(target),
            "the runbooks map `{source}` to `{target}`, which is not a canonical stage"
        );
        assert!(
            known.contains(source),
            "the runbooks map `{source}` to `{target}`, but no harness runs a \
             stage called `{source}`"
        );
    }
}

/// Every stage name the two physical harnesses actually run.
fn harness_stage_names() -> std::collections::BTreeSet<String> {
    let mut names = std::collections::BTreeSet::new();
    let sources = [
        (
            "tools/validation/macos-homebrew-lifecycle.sh",
            vec!["run_stage"],
        ),
        (
            "tools/validation/jetson-clean-room.sh",
            vec!["required", "optional", "capture"],
        ),
    ];
    for (path, keywords) in sources {
        let body = std::fs::read_to_string(repo_root().join(path))
            .unwrap_or_else(|e| panic!("read {path}: {e}"));
        for line in body.lines() {
            let mut tokens = line.split_whitespace();
            let Some(first) = tokens.next() else { continue };
            if !keywords.contains(&first) {
                continue;
            }
            if let Some(name) = tokens.next() {
                if name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                {
                    names.insert(name.to_string());
                }
            }
        }
    }
    assert!(
        names.len() > 20,
        "expected to find the harnesses' stage names, found {names:?}"
    );
    names
}
