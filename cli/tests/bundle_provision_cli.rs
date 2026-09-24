// SPDX-License-Identifier: Apache-2.0
//
// `tensorplate bundle provision` through the binary: argument parsing, the
// JSON envelope, exit code 12 for a provisioning failure, and the refusal
// to route it to a device. The outcomes themselves are covered in
// bundle_provision.rs.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use std::path::PathBuf;

use serde_json::Value;

fn repo(relative: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
        .display()
        .to_string()
}

fn run(args: &[&str]) -> (i32, String, String) {
    // No agent is involved; the socket path is never opened.
    common::run_cli(std::path::Path::new("/nonexistent/agent.sock"), args)
}

#[test]
fn a_provisioned_bundle_is_reported_in_the_json_envelope() {
    let into = tempfile::tempdir().unwrap();
    let source = repo("test/models/bundles/v0_1/smolvla_python_pytorch");
    let manifest = repo("protocol/rust/tests/fixtures/provisioning_manifest.json");
    let into_path = into.path().display().to_string();
    let (code, stdout, stderr) = run(&[
        "--output",
        "json",
        "bundle",
        "provision",
        "smolvla-fixture",
        "--from",
        &source,
        "--manifest",
        &manifest,
        "--into",
        &into_path,
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let envelope: Value = serde_json::from_str(&stdout).expect("one JSON envelope");
    assert_eq!(envelope["command"], "bundle");
    assert_eq!(envelope["status"], "ok");
    assert_eq!(envelope["payload"]["outcome"], "provisioned");
    assert_eq!(envelope["payload"]["files"], 4);
}

#[test]
fn a_provisioning_failure_exits_12_with_its_code() {
    let into = tempfile::tempdir().unwrap();
    let empty_source = tempfile::tempdir().unwrap();
    let manifest = repo("protocol/rust/tests/fixtures/provisioning_manifest.json");
    let (from, into_path) = (
        empty_source.path().display().to_string(),
        into.path().display().to_string(),
    );
    let (code, _stdout, stderr) = run(&[
        "--output",
        "json",
        "bundle",
        "provision",
        "smolvla-fixture",
        "--from",
        &from,
        "--manifest",
        &manifest,
        "--into",
        &into_path,
    ]);
    assert_eq!(code, 12, "stderr: {stderr}");
    let envelope: Value = serde_json::from_str(&stderr).expect("one JSON error envelope");
    assert_eq!(envelope["error"]["context"], "source_missing");
    assert_eq!(envelope["error"]["code"], "load_failed");
}

#[test]
fn provisioning_is_never_routed_to_a_device_and_needs_a_source() {
    let (code, _stdout, stderr) = run(&[
        "--device",
        "orin",
        "bundle",
        "provision",
        "smolvla-fixture",
        "--from",
        "/tmp",
    ]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("run it on the device"), "{stderr}");

    let (code, _stdout, stderr) = run(&["bundle", "provision", "smolvla-fixture"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("requires `--from <dir>`"), "{stderr}");

    let (code, _stdout, stderr) = run(&["bundle", "fetch"]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("unknown bundle subcommand `fetch`"),
        "{stderr}"
    );
}
