// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F08 CLI integration: infer convenience workflow.
//
// Exercises the data-plane HTTP call against a stub serving worker plus
// the agent stub that surfaces the active deployment for endpoint
// resolution.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access
)]

mod common;

use common::{run_cli, AgentStub, ServingStub};
use tensorplate_protocol::agent_control::{
    AgentRunState, AgentStatus, ControlResponse, DeploymentSummary,
};

fn active_deployment_response() -> ControlResponse {
    let status = AgentStatus {
        agent_state: AgentRunState::Ready,
        active: Some(DeploymentSummary {
            deployment_id: "d-1".into(),
            bundle_digest: "sha256:abc".into(),
            bundle_name: None,
            bundle_version: None,
            backend_hint: Some("mock".into()),
            model_class: None,
            staged_path: None,
            promoted_monotonic_ns: Some(1),
            serving_url: None,
        }),
        previous_active: None,
        candidate: None,
        in_flight_transaction: None,
        last_error: None,
        quarantined: vec![],
        recovery: None,
        supervision: None,
    };
    ControlResponse {
        agent_status: Some(status),
        ..ControlResponse::ok(Some("c".into()))
    }
}

#[test]
fn infer_with_serving_url_flag_does_not_call_agent() {
    let stub = AgentStub::start();
    let success_body = r#"{"schema_version":"0.1","status":"success","request_id":"r-1","outputs":[{"name":"boxes","tensor":{"dtype":"float32","shape":[1,4]},"payload_b64":""}]}"#;
    let serving = ServingStub::start(success_body);
    let td = tempfile::tempdir().unwrap();
    let input = td.path().join("input.json");
    std::fs::write(&input, r#"{"inputs":[]}"#).unwrap();
    let (code, stdout, _stderr) = run_cli(
        &stub.socket,
        &[
            "--output",
            "json",
            "infer",
            "--input",
            input.to_str().unwrap(),
            "--serving-url",
            &serving.url(),
        ],
    );
    assert_eq!(code, 0, "stdout was: {stdout}");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(parsed["status"], "ok");
    assert_eq!(parsed["payload"]["endpoint_source"], "flag");
    assert_eq!(parsed["payload"]["result"]["request_id"], "r-1");
    // No agent calls when the flag is supplied.
    assert!(stub.history().is_empty());
    // Stub serving received a POST.
    let req_log = serving.requests();
    assert_eq!(req_log.len(), 1);
    let request_str = String::from_utf8_lossy(&req_log[0]);
    assert!(request_str.contains("POST /infer"));
    assert!(request_str.contains("X-Correlation-Id"));
}

#[test]
fn infer_falls_back_to_agent_discovered_endpoint_when_no_overrides() {
    let stub = AgentStub::start();
    stub.enqueue(active_deployment_response());
    // No serving stub running; we should fail because the discovered
    // endpoint (127.0.0.1:18080) is unreachable, but the test asserts
    // the agent was actually consulted for endpoint resolution.
    let td = tempfile::tempdir().unwrap();
    let input = td.path().join("input.json");
    std::fs::write(&input, r#"{"inputs":[]}"#).unwrap();
    let (code, _stdout, _stderr) =
        run_cli(&stub.socket, &["infer", "--input", input.to_str().unwrap()]);
    // Connection refused → transport exit code.
    assert_eq!(code, 4);
    let history = stub.history();
    assert_eq!(
        history.len(),
        1,
        "agent should have been consulted for active deployment"
    );
}

#[test]
fn infer_returns_unavailable_when_no_active_deployment() {
    let stub = AgentStub::start();
    let mut response = ControlResponse::ok(Some("c".into()));
    response.agent_status = Some(AgentStatus {
        agent_state: AgentRunState::Ready,
        active: None,
        previous_active: None,
        candidate: None,
        in_flight_transaction: None,
        last_error: None,
        quarantined: vec![],
        recovery: None,
        supervision: None,
    });
    stub.enqueue(response);
    let td = tempfile::tempdir().unwrap();
    let input = td.path().join("input.json");
    std::fs::write(&input, r#"{"inputs":[]}"#).unwrap();
    let (code, _stdout, _stderr) =
        run_cli(&stub.socket, &["infer", "--input", input.to_str().unwrap()]);
    assert_eq!(code, 6);
}

#[test]
fn infer_rejects_malformed_input() {
    let stub = AgentStub::start();
    let td = tempfile::tempdir().unwrap();
    let input = td.path().join("input.json");
    std::fs::write(&input, b"not json").unwrap();
    let (code, _stdout, _stderr) =
        run_cli(&stub.socket, &["infer", "--input", input.to_str().unwrap()]);
    assert_eq!(code, 2);
    assert!(stub.history().is_empty());
}
