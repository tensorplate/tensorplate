// SPDX-License-Identifier: Apache-2.0
//
// The shared error-code enum and the failure-reason taxonomy against every
// schema that carries a copy of them.
//
// `protocol/schemas/error.json` is canonical for the error-code enum, but
// most schemas inline their own copy instead of referencing it. This suite
// walks every schema file and holds each copy, in order, to
// `ErrorCode::ALL`, and holds the enums in `failure_reason.json` and the
// reason table in `docs/observability/failure-reasons.md` to the Rust
// taxonomy, so a value appended to one copy alone fails here. The C++ and
// Python mirrors are checked against `error.json` by their own suites.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use serde_json::Value;
use tensorplate_protocol::{
    ErrorCode, FailureCategory, FailureReason, FailureReasonRecord, FailureSeverity, ProtocolError,
};

fn repo_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn load(path: &Path) -> Value {
    let raw =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn schema_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in ["protocol/schemas", "config/schemas"] {
        let entries = std::fs::read_dir(repo_root().join(dir)).expect("read schema dir");
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Every `enum` array under `value`, with its JSON pointer.
fn enums<'a>(value: &'a Value, pointer: &str, out: &mut Vec<(String, &'a Vec<Value>)>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let child_pointer = format!("{pointer}/{key}");
                if key == "enum" {
                    if let Value::Array(items) = child {
                        out.push((child_pointer.clone(), items));
                    }
                }
                enums(child, &child_pointer, out);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                enums(child, &format!("{pointer}/{index}"), out);
            }
        }
        _ => {}
    }
}

fn strings(values: impl IntoIterator<Item = String>) -> Vec<Value> {
    values.into_iter().map(Value::String).collect()
}

/// Every schema location that carries a copy of the error-code enum. A copy
/// that is added, moved or replaced by a `$ref` changes this list on purpose.
const ERROR_CODE_COPIES: [&str; 18] = [
    "agent_control.json#/definitions/DeployStatus/properties/failure/properties/error_code/enum",
    "agent_control.json#/definitions/Error/properties/code/enum",
    "agent_control.json#/definitions/QuarantineRecord/properties/error_code/enum",
    "agent_control.json#/definitions/SupervisionStatus/properties/last_failure_code/enum",
    "agent_state.json#/definitions/ErrorRecord/properties/code/enum",
    "cli_output.json#/properties/error/properties/code/enum",
    "deploy_transaction.json#/properties/failure/properties/error_code/enum",
    "error.json#/properties/code/enum",
    "failure_reason.json#/properties/error_code/enum",
    "health_event.json#/properties/error_code/enum",
    "log_event.json#/properties/error_code/enum",
    "observability_status.json#/properties/last_error_code/enum",
    "safe_state_event.json#/properties/last_error_code/enum",
    "scheduler_event.json#/properties/error_code/enum",
    "serving_health.json#/properties/last_error_code/enum",
    "supervision_event.json#/properties/error_code/enum",
    "worker_control.json#/definitions/Response/properties/error/properties/code/enum",
    "worker_status.json#/properties/last_error_code/enum",
];

/// Schema locations that pin a legacy vocabulary on purpose: a strict prefix
/// of the error codes, frozen at what agents through 0.2.x decode. They are
/// not copies of the live enum and never gain an appended value.
const FROZEN_ERROR_CODE_PREFIXES: [&str; 1] =
    ["agent_state.json#/definitions/LegacyErrorRecord/properties/code/enum"];

fn error_code_names() -> Vec<Value> {
    strings(ErrorCode::ALL.map(|c| c.as_str().to_owned()))
}

#[test]
fn every_inline_error_code_enum_matches_error_code_all() {
    let expected = error_code_names();
    let mut copies = Vec::new();
    let mut frozen = Vec::new();
    for path in schema_files() {
        let schema = load(&path);
        let mut found = Vec::new();
        enums(&schema, "", &mut found);
        let name = path.file_name().expect("file name").to_string_lossy();
        for (pointer, items) in found {
            // An error-code copy is any enum holding one of the original
            // names; it must hold every name, in order, and nothing else,
            // unless it is a pinned legacy vocabulary, which holds a strict
            // prefix of the names on purpose and is listed as such.
            if items.iter().any(|v| v == "inference_failed") {
                let location = format!("{name}#{pointer}");
                if FROZEN_ERROR_CODE_PREFIXES.contains(&location.as_str()) {
                    let pinned: &[Value] = items;
                    assert!(
                        pinned.len() < expected.len() && expected.starts_with(pinned),
                        "{name}{pointer} pins a legacy vocabulary and must be a strict prefix of ErrorCode::ALL"
                    );
                    frozen.push(location);
                    continue;
                }
                assert_eq!(
                    items, &expected,
                    "{name}{pointer} does not match ErrorCode::ALL in order"
                );
                copies.push(location);
            }
        }
    }
    // The walk found exactly the known copies and pinned vocabularies: none
    // vanished, so none escaped the checks above, and a new one is added to
    // its list.
    copies.sort();
    assert_eq!(copies, ERROR_CODE_COPIES, "error-code enum copies moved");
    frozen.sort();
    assert_eq!(
        frozen, FROZEN_ERROR_CODE_PREFIXES,
        "pinned legacy vocabularies moved"
    );
}

#[test]
fn failure_reason_schema_enums_match_the_rust_taxonomy() {
    let schema = load(&repo_root().join("protocol/schemas/failure_reason.json"));
    let properties = &schema["properties"];

    let reasons = strings(FailureReason::ALL.map(|r| r.as_str().to_owned()));
    assert_eq!(properties["reason"]["enum"], Value::Array(reasons));

    let categories: Vec<Value> = FailureCategory::ALL
        .iter()
        .map(|c| serde_json::to_value(c).expect("category"))
        .collect();
    assert_eq!(properties["category"]["enum"], Value::Array(categories));

    let severities: Vec<Value> = [
        FailureSeverity::Warning,
        FailureSeverity::Error,
        FailureSeverity::Critical,
    ]
    .iter()
    .map(|s| serde_json::to_value(s).expect("severity"))
    .collect();
    assert_eq!(properties["severity"]["enum"], Value::Array(severities));
}

#[test]
fn every_canonical_failure_record_satisfies_the_schema() {
    let schema = load(&repo_root().join("protocol/schemas/failure_reason.json"));
    let validator = jsonschema::JSONSchema::compile(&schema).expect("schema compiles");
    for reason in FailureReason::ALL {
        let record = serde_json::to_value(FailureReasonRecord::new(reason)).expect("record");
        assert!(
            validator.is_valid(&record),
            "{reason}: canonical record violates failure_reason.json: {record}"
        );
    }
}

#[test]
fn every_error_code_payload_satisfies_the_error_schema() {
    let schema = load(&repo_root().join("protocol/schemas/error.json"));
    let validator = jsonschema::JSONSchema::compile(&schema).expect("schema compiles");
    for code in ErrorCode::ALL {
        let payload = serde_json::to_value(ProtocolError::new(code, "m")).expect("payload");
        assert!(
            validator.is_valid(&payload),
            "{code}: payload violates error.json: {payload}"
        );
    }
    let unknown = serde_json::json!({
        "schema_version": tensorplate_protocol::SCHEMA_VERSION,
        "code": "canceled",
        "message": "m",
    });
    assert!(
        !validator.is_valid(&unknown),
        "a misspelled code must be rejected"
    );
}

#[test]
fn failure_reason_doc_table_matches_the_canonical_records() {
    let doc = std::fs::read_to_string(repo_root().join("docs/observability/failure-reasons.md"))
        .expect("read failure-reasons.md");
    let section = doc
        .split("## Reasons")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .expect("## Reasons section");
    let rows: Vec<Vec<String>> = section
        .lines()
        .filter(|line| line.starts_with("| `"))
        .map(|line| {
            line.trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().trim_matches('`').to_owned())
                .collect()
        })
        .collect();
    assert_eq!(
        rows.len(),
        FailureReason::ALL.len(),
        "one table row per reason"
    );
    for (row, reason) in rows.iter().zip(FailureReason::ALL) {
        let record = FailureReasonRecord::new(reason);
        let as_wire = |value: Value| value.as_str().expect("string").to_owned();
        let expected = vec![
            reason.as_str().to_owned(),
            as_wire(serde_json::to_value(record.category).expect("category")),
            as_wire(serde_json::to_value(record.severity).expect("severity")),
            if record.retryable { "yes" } else { "no" }.to_owned(),
            record.error_code.as_str().to_owned(),
        ];
        assert_eq!(row, &expected, "failure-reasons.md row for {reason}");
    }
}
