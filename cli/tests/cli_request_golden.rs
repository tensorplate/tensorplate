// SPDX-License-Identifier: Apache-2.0
//
// The CLI's deploy and rollback requests, byte for byte.
//
// `protocol/rust/tests/fixtures/agent_control_singleton_lifecycle.jsonl`
// holds the request lines a default deploy and a rollback put on the
// agent socket, recorded before the set-mutation fields were added. The
// binary must still send exactly those bytes when no set operation,
// member or operator-only field is asked for. The correlation id (random
// per run) and the bundle path (a temporary directory) are normalized.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use common::{run_cli, write_bundle_dir, AgentStub};
use std::path::PathBuf;
use tensorplate_protocol::agent_control::ControlResponse;

fn golden_line(index: usize) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../protocol/rust/tests/fixtures/agent_control_singleton_lifecycle.jsonl");
    let text = std::fs::read_to_string(path).expect("read golden");
    text.lines().nth(index).expect("golden line").to_string()
}

/// The single request the CLI sent, with its correlation id replaced.
fn sent(stub: &AgentStub, correlation: &str) -> String {
    let raw = stub.raw_history();
    assert_eq!(raw.len(), 1, "one request per command");
    let value: serde_json::Value = serde_json::from_str(&raw[0]).expect("json");
    let generated = value["correlation_id"].as_str().expect("correlation id");
    raw[0].replace(generated, correlation)
}

#[test]
fn a_default_deploy_sends_the_recorded_bytes() {
    let stub = AgentStub::start();
    let (_td, bundle) = write_bundle_dir();
    stub.enqueue(ControlResponse::ok(None));
    let bundle = bundle.canonicalize().expect("canonical bundle");
    let (_code, _stdout, stderr) = run_cli(
        &stub.socket,
        &[
            "deploy",
            bundle.to_str().unwrap(),
            "--deployment-id",
            "d1",
            "--label",
            "team=vision",
            "--no-wait",
        ],
    );
    let line =
        sent(&stub, "ctl-000001").replace(&bundle.display().to_string(), "/srv/golden/bundle-d1");
    assert_eq!(line, golden_line(0), "stderr was: {stderr}");
}

#[test]
fn a_rollback_sends_the_recorded_bytes() {
    let stub = AgentStub::start();
    stub.enqueue(ControlResponse::ok(None));
    let (_code, _stdout, stderr) =
        run_cli(&stub.socket, &["rollback", "--reason", "operator rollback"]);
    assert_eq!(
        sent(&stub, "ctl-000004"),
        golden_line(6),
        "stderr was: {stderr}"
    );
}
