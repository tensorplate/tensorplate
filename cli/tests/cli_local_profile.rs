// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F08 CLI integration: local UDS profile.
//
// Exercises the full binary against a stub agent: status (ready), deploy
// (no-wait), rollback (success and unavailable), and doctor with a
// pre-recorded agent_status.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access
)]

mod common;

use common::{run_cli, write_bundle_dir, AgentStub};
use tensorplate_protocol::agent_control::{
    AgentRunState, AgentStatus, ControlResponse, DeployStatus, DeploymentSummary, ResponseError,
    SupervisionStatusSummary,
};
use tensorplate_protocol::deploy_transaction::DeployState;
use tensorplate_protocol::supervision_event::{SupervisionAgentState, SupervisionServingState};
use tensorplate_protocol::ErrorCode;

fn ready_status_response() -> ControlResponse {
    let status = AgentStatus {
        agent_state: AgentRunState::Ready,
        active: Some(DeploymentSummary {
            deployment_id: "d-1".into(),
            bundle_digest: "sha256:abc".into(),
            bundle_name: Some("yolov8n".into()),
            bundle_version: Some("0.0.1".into()),
            backend_hint: Some("tensorrt".into()),
            model_class: Some("vision".into()),
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
        supervision: Some(SupervisionStatusSummary {
            serving_state: SupervisionServingState::Ready,
            agent_state: SupervisionAgentState::Ready,
            desired_active: Some("d-1".into()),
            actual_active: Some("d-1".into()),
            backend: Some("tensorrt".into()),
            restart_count: 0,
            crash_loop_threshold: 5,
            crash_loop: false,
            launch_sequence: 1,
            last_failure_code: None,
            last_failure_message: None,
            next_restart_delay_ms: None,
            stable_uptime_ms: 30_000,
        }),
        platform_telemetry: None,
    };
    ControlResponse {
        agent_status: Some(status),
        ..ControlResponse::ok(Some("c".into()))
    }
}

fn deploy_ok_response() -> ControlResponse {
    let status = DeployStatus {
        phase: DeployState::Received,
        transaction_id: Some("tx-1".into()),
        deployment_id: Some("d-2".into()),
        bundle_digest: Some("sha256:def".into()),
        started_monotonic_ns: Some(1),
        last_transition_monotonic_ns: Some(2),
        failure: None,
    };
    ControlResponse {
        transaction_id: Some("tx-1".into()),
        deploy_status: Some(status),
        ..ControlResponse::ok(Some("c".into()))
    }
}

#[test]
fn version_runs_without_agent() {
    let stub = AgentStub::start();
    let (code, stdout, _stderr) = run_cli(&stub.socket, &["version"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("tensorplate "));
}

#[test]
fn status_command_round_trips_against_local_stub() {
    let stub = AgentStub::start();
    stub.enqueue(ready_status_response());
    let (code, stdout, _stderr) = run_cli(&stub.socket, &["--output", "json", "status"]);
    assert_eq!(code, 0, "stdout was: {stdout}");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(parsed["status"], "ok");
    assert_eq!(parsed["payload"]["agent"]["active"]["deployment_id"], "d-1");
    assert_eq!(parsed["payload"]["severity"], "ready");
    let history = stub.history();
    assert_eq!(history.len(), 1);
}

#[test]
fn deploy_no_wait_submits_bundle_through_agent() {
    let stub = AgentStub::start();
    let (_td, bundle) = write_bundle_dir();
    stub.enqueue(deploy_ok_response());
    let (code, stdout, _stderr) = run_cli(
        &stub.socket,
        &[
            "--output",
            "json",
            "deploy",
            bundle.to_str().unwrap(),
            "--deployment-id",
            "d-2",
            "--no-wait",
        ],
    );
    assert_eq!(code, 0, "stdout was: {stdout}");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(parsed["payload"]["transaction_id"], "tx-1");
    assert_eq!(parsed["payload"]["phase"], "received");
    let history = stub.history();
    assert!(history[0].deploy.is_some());
}

#[test]
fn rollback_unavailable_returns_unavailable_exit_code() {
    let stub = AgentStub::start();
    stub.enqueue(ControlResponse::unavailable(
        Some("c".into()),
        "no previous active deployment",
    ));
    let (code, _stdout, stderr) = run_cli(&stub.socket, &["rollback"]);
    assert_eq!(code, 6, "stderr was: {stderr}");
}

#[test]
fn agent_error_returns_agent_error_exit_code() {
    let stub = AgentStub::start();
    stub.enqueue(ControlResponse::error(
        Some("c".into()),
        ResponseError::new(ErrorCode::Unsupported, "bundle backend unavailable"),
    ));
    let (_td, bundle) = write_bundle_dir();
    let (code, _stdout, stderr) = run_cli(
        &stub.socket,
        &[
            "deploy",
            bundle.to_str().unwrap(),
            "--deployment-id",
            "d-3",
            "--no-wait",
        ],
    );
    assert_eq!(code, 3, "stderr was: {stderr}");
}

#[test]
fn doctor_reports_active_deployment_when_present() {
    let stub = AgentStub::start();
    // doctor sends: version, then status.
    stub.enqueue(ControlResponse::ok(Some("v".into())));
    stub.enqueue(ready_status_response());
    let (code, stdout, _stderr) = run_cli(&stub.socket, &["--output", "json", "doctor"]);
    assert_eq!(code, 0, "stdout was: {stdout}");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let findings = parsed["payload"]["findings"].as_array().expect("array");
    assert!(findings
        .iter()
        .any(|f| f["id"] == "active_deployment" && f["status"] == "ok"));
}

#[test]
fn doctor_skip_agent_does_not_call_agent() {
    let stub = AgentStub::start();
    // No responses enqueued; doctor must succeed without calling the agent.
    let (code, _stdout, _stderr) = run_cli(&stub.socket, &["doctor", "--skip-agent"]);
    // Doctor exits 0 because nothing has Fail severity in skip-agent mode.
    assert_eq!(code, 0);
    assert!(stub.history().is_empty());
}

#[test]
fn unknown_subcommand_returns_usage_exit_code() {
    let stub = AgentStub::start();
    let (code, _stdout, _stderr) = run_cli(&stub.socket, &["frob"]);
    assert_eq!(code, 2);
}
