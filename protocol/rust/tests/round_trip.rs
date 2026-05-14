// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F07-T07: canonical protocol fixtures.
//
// The committed JSON files under `tests/fixtures/` are the canonical
// contract for v0.1 protocol payloads. This suite validates the Rust side
// of the fixture contract; C++ and Python binding round trips pick up
// these same fixtures when their JSON/IPC bindings land in V01-E07 /
// V01-E05.
//
// What "round trip" means here:
//   1. Read the fixture from disk.
//   2. Decode it via `decode_with_version_check` (typed schema-version
//      enforcement).
//   3. Re-serialize the typed value back to JSON.
//   4. Re-decode the re-serialized JSON.
//   5. Assert structural equality between the two typed values.
//
// This proves that:
//   - The schema-version enforcement is wired end-to-end.
//   - The Rust struct is a lossless representation of the wire payload.
//   - The fixture is parseable on a clean checkout (catches accidental
//     manual edits that drift from the schema).

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use tensorplate_protocol::{
    decode_with_version_check, DecodeError, DeployTransaction, DesiredState, HealthEvent,
    IpcMessage, ValidatePayload, WorkerStatus, SCHEMA_VERSION,
};

fn fixtures_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p
}

fn load(name: &str) -> String {
    let mut p = fixtures_dir();
    p.push(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
}

fn round_trip<T>(name: &str)
where
    T: serde::de::DeserializeOwned
        + serde::Serialize
        + std::fmt::Debug
        + PartialEq
        + ValidatePayload,
{
    let raw = load(name);
    let first: T = decode_with_version_check(&raw)
        .unwrap_or_else(|e| panic!("decode-with-version-check failed for {name}: {e}"));
    let re_emitted = serde_json::to_string(&first)
        .unwrap_or_else(|e| panic!("serialize failed for {name}: {e}"));
    let second: T = decode_with_version_check(&re_emitted)
        .unwrap_or_else(|e| panic!("re-decode failed for {name}: {e}\nemitted JSON: {re_emitted}"));
    assert_eq!(
        first, second,
        "round-trip mismatch for {name}\nemitted JSON: {re_emitted}"
    );
}

#[test]
fn desired_state_vision_round_trip() {
    round_trip::<DesiredState>("desired_state_vision.json");
}

#[test]
fn desired_state_smolvla_round_trip() {
    round_trip::<DesiredState>("desired_state_smolvla.json");
}

#[test]
fn worker_status_ready_round_trip() {
    round_trip::<WorkerStatus>("worker_status_ready.json");
}

#[test]
fn worker_status_degraded_round_trip() {
    round_trip::<WorkerStatus>("worker_status_degraded.json");
}

#[test]
fn health_event_missed_deadline_round_trip() {
    round_trip::<HealthEvent>("health_event_missed_deadline.json");
}

#[test]
fn deploy_transaction_active_round_trip() {
    round_trip::<DeployTransaction>("deploy_transaction_active.json");
}

#[test]
fn deploy_transaction_failed_round_trip() {
    round_trip::<DeployTransaction>("deploy_transaction_failed.json");
}

#[test]
fn python_pytorch_ipc_load_model_round_trip() {
    round_trip::<IpcMessage>("python_pytorch_ipc_load_model.json");
}

#[test]
fn python_pytorch_ipc_health_check_response_round_trip() {
    round_trip::<IpcMessage>("python_pytorch_ipc_health_check_response.json");
}

#[test]
fn unknown_schema_version_is_rejected_with_typed_error() {
    let raw = load("python_pytorch_ipc_unknown_version.json");
    let err = decode_with_version_check::<IpcMessage>(&raw).expect_err("must reject");
    match err {
        DecodeError::UnsupportedSchemaVersion { got, expected } => {
            assert_eq!(got, "99.99");
            assert_eq!(expected, SCHEMA_VERSION);
        }
        other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
    }
}

#[test]
fn fixtures_match_committed_schema_version() {
    // Sanity: every fixture must declare the v0.1 schema version, except
    // the explicit unknown-version negative fixture. This guards against
    // accidentally checking in a fixture against an unreleased schema.
    let dir = fixtures_dir();
    let entries = std::fs::read_dir(dir).expect("read fixtures dir");
    let mut count = 0usize;
    for entry in entries {
        let entry = entry.expect("entry");
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .expect("name");
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(&path).expect("read");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("parse");
        let observed = v
            .get("schema_version")
            .and_then(serde_json::Value::as_str)
            .expect("schema_version field");
        if name == "python_pytorch_ipc_unknown_version.json" {
            assert_ne!(observed, SCHEMA_VERSION);
        } else {
            assert_eq!(
                observed, SCHEMA_VERSION,
                "fixture `{name}` declares an unexpected schema_version"
            );
        }
        count += 1;
    }
    assert!(count >= 8, "expected at least 8 fixtures, found {count}");
}
