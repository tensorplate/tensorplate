// SPDX-License-Identifier: Apache-2.0
//
// The CLI's set-mutation commands and flags against the stub agent.
//
// `undeploy` and `recover` send their member and surface the agent's typed
// answer. `deploy --set-operation add` and `rollback --deployment-id` add
// fields an agent that predates them would ignore, so the CLI first asks
// for the agent's control features and sends nothing further unless the
// matching feature is listed. `status` renders the resident set when the
// agent reports one.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use common::{run_cli, write_bundle_dir, AgentStub};
use serde_json::Value;
use tensorplate_protocol::agent_control::{
    AgentRunState, AgentStatus, ControlOp, ControlResponse, ResidentSetStatus, ResponseError,
    FEATURE_MEMBER_ROLLBACK, FEATURE_SET_OPERATION_ADD,
};
use tensorplate_protocol::agent_state::decode_agent_state;
use tensorplate_protocol::ErrorCode;

fn unsupported(what: &str) -> ControlResponse {
    ControlResponse::error(
        Some("c".into()),
        ResponseError::new(
            ErrorCode::Unsupported,
            format!("{what} is not supported by this agent yet"),
        ),
    )
}

fn status_listing(features: &[&str]) -> ControlResponse {
    ControlResponse {
        agent_status: Some(AgentStatus {
            agent_state: AgentRunState::Ready,
            control_features: features.iter().map(ToString::to_string).collect(),
            ..AgentStatus::default()
        }),
        ..ControlResponse::ok(Some("c".into()))
    }
}

fn sent_json(stub: &AgentStub) -> Vec<Value> {
    stub.raw_history()
        .iter()
        .map(|line| serde_json::from_str(line).expect("json"))
        .collect()
}

#[test]
fn undeploy_and_recover_send_their_member_and_surface_unsupported() {
    for (command, op) in [("undeploy", "undeploy"), ("recover", "recover")] {
        let stub = AgentStub::start();
        stub.enqueue(unsupported(&format!("`{op}`")));
        let (code, _stdout, stderr) = run_cli(
            &stub.socket,
            &[
                "--output",
                "json",
                command,
                "--deployment-id",
                "speech-tts",
                "--reason",
                "retired",
            ],
        );
        assert_eq!(code, 3, "{command}: stderr was {stderr}");
        // Error envelopes go to stderr.
        let body: Value = serde_json::from_str(&stderr).expect("json envelope");
        assert_eq!(body["command"], command);
        assert_eq!(body["error"]["code"], "unsupported");
        let sent = sent_json(&stub);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["op"], op);
        assert_eq!(
            sent[0][op],
            serde_json::json!({"deployment_id": "speech-tts", "reason": "retired"})
        );
    }
}

#[test]
fn a_member_command_needs_a_deployment_id() {
    let stub = AgentStub::start();
    let (code, _stdout, stderr) = run_cli(&stub.socket, &["undeploy"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--deployment-id"), "{stderr}");
    assert!(stub.raw_history().is_empty());
}

#[test]
fn set_operation_add_is_refused_locally_unless_the_agent_lists_it() {
    let (_td, bundle) = write_bundle_dir();
    let bundle = bundle.to_str().unwrap().to_string();
    let args = [
        "--output",
        "json",
        "deploy",
        bundle.as_str(),
        "--deployment-id",
        "speech-tts",
        "--set-operation",
        "add",
        "--no-wait",
    ];

    let stub = AgentStub::start();
    stub.enqueue(status_listing(&[FEATURE_MEMBER_ROLLBACK]));
    let (code, _stdout, stderr) = run_cli(&stub.socket, &args);
    assert_eq!(code, 3, "stderr was {stderr}");
    let body: Value = serde_json::from_str(&stderr).expect("json envelope");
    assert_eq!(body["error"]["code"], "unsupported");
    let sent = sent_json(&stub);
    assert_eq!(sent.len(), 1, "only the feature query was sent");
    assert_eq!(sent[0]["op"], "status");

    let stub = AgentStub::start();
    stub.enqueue(status_listing(&[FEATURE_SET_OPERATION_ADD]));
    stub.enqueue(ControlResponse {
        transaction_id: Some("tx-1".into()),
        ..ControlResponse::ok(Some("c".into()))
    });
    let (code, _stdout, stderr) = run_cli(&stub.socket, &args);
    assert_eq!(code, 0, "stderr was {stderr}");
    let sent = sent_json(&stub);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["op"], "deploy");
    assert_eq!(sent[1]["deploy"]["set_operation"], "add");
}

#[test]
fn an_explicit_replace_sends_the_default_request() {
    let stub = AgentStub::start();
    stub.enqueue(ControlResponse::ok(Some("c".into())));
    let (_td, bundle) = write_bundle_dir();
    let (_code, _stdout, stderr) = run_cli(
        &stub.socket,
        &[
            "deploy",
            bundle.to_str().unwrap(),
            "--deployment-id",
            "d1",
            "--set-operation",
            "replace",
            "--no-wait",
        ],
    );
    let sent = stub.raw_history();
    assert_eq!(sent.len(), 1, "no feature query for the default: {stderr}");
    assert!(
        !sent[0].contains("set_operation"),
        "`replace` is sent as an absent field: {}",
        sent[0]
    );
}

#[test]
fn an_unknown_set_operation_is_a_usage_error() {
    let stub = AgentStub::start();
    let (_td, bundle) = write_bundle_dir();
    let (code, _stdout, stderr) = run_cli(
        &stub.socket,
        &[
            "deploy",
            bundle.to_str().unwrap(),
            "--set-operation",
            "merge",
        ],
    );
    assert_eq!(code, 2, "{stderr}");
    assert!(stub.raw_history().is_empty());
}

#[test]
fn a_member_rollback_is_refused_locally_unless_the_agent_lists_it() {
    let stub = AgentStub::start();
    stub.enqueue(status_listing(&[]));
    let (code, _stdout, stderr) =
        run_cli(&stub.socket, &["rollback", "--deployment-id", "speech-tts"]);
    assert_eq!(code, 3, "stderr was {stderr}");
    let sent = sent_json(&stub);
    assert_eq!(sent.len(), 1, "only the feature query was sent");
    assert_eq!(sent[0]["op"], "status");

    let stub = AgentStub::start();
    stub.enqueue(status_listing(&[FEATURE_MEMBER_ROLLBACK]));
    stub.enqueue(ControlResponse::ok(Some("c".into())));
    let (code, _stdout, stderr) =
        run_cli(&stub.socket, &["rollback", "--deployment-id", "speech-tts"]);
    assert_eq!(code, 0, "stderr was {stderr}");
    let sent = sent_json(&stub);
    assert_eq!(sent[1]["op"], "rollback");
    assert_eq!(sent[1]["rollback"]["deployment_id"], "speech-tts");
    assert_eq!(stub.history()[1].op, ControlOp::Rollback);
}

fn two_member_status(quarantine_second: bool) -> ControlResponse {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../protocol/rust/tests/fixtures/agent_state_0_2_two_member_set.json");
    let mut set = decode_agent_state(&std::fs::read_to_string(path).expect("fixture"))
        .expect("decodes")
        .resident_set
        .expect("set");
    if quarantine_second {
        set.members[1].state = tensorplate_protocol::MemberState::Quarantined;
        set.endpoint_map.truncate(1);
    }
    ControlResponse {
        agent_status: Some(AgentStatus {
            agent_state: AgentRunState::Ready,
            resident_set: Some(ResidentSetStatus::from_committed(&set)),
            ..AgentStatus::default()
        }),
        ..ControlResponse::ok(Some("c".into()))
    }
}

#[test]
fn status_renders_the_resident_set() {
    let stub = AgentStub::start();
    stub.enqueue(two_member_status(false));
    let (code, stdout, stderr) = run_cli(&stub.socket, &["--output", "json", "status"]);
    assert_eq!(code, 0, "stderr was {stderr}");
    let body: Value = serde_json::from_str(&stdout).expect("json envelope");
    let members = body["payload"]["agent"]["resident_set"]["members"]
        .as_array()
        .expect("members");
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["deployment_id"], "speech-stt");
    assert_eq!(members[1]["stream_endpoint"], "127.0.0.1:18105");
    assert_eq!(body["payload"]["severity"], "ready");
    // The singleton keys stay, null, for readers that test for them.
    assert!(body["payload"]["agent"]["active"].is_null());
    assert!(body["payload"]["agent"]
        .as_object()
        .expect("agent")
        .contains_key("active"));

    let stub = AgentStub::start();
    stub.enqueue(two_member_status(true));
    let (_code, stdout, _stderr) = run_cli(&stub.socket, &["status"]);
    assert!(
        stdout.contains("resident_set: set_id="),
        "human output: {stdout}"
    );
    assert!(
        stdout.contains("member: deployment_id=speech-tts generation=5 state=quarantined"),
        "human output: {stdout}"
    );
    assert!(
        stdout.contains("severity=degraded"),
        "human output: {stdout}"
    );
}

#[test]
fn status_without_a_resident_set_is_unchanged() {
    let stub = AgentStub::start();
    stub.enqueue(status_listing(&[]));
    let (_code, stdout, _stderr) = run_cli(&stub.socket, &["--output", "json", "status"]);
    let body: Value = serde_json::from_str(&stdout).expect("json envelope");
    assert!(!body["payload"]["agent"]
        .as_object()
        .expect("agent")
        .contains_key("resident_set"));
}

fn member(
    id: &str,
    generation: u64,
    admission: tensorplate_protocol::AdmissionMode,
    contact: tensorplate_protocol::ContactState,
) -> tensorplate_protocol::MemberStatus {
    tensorplate_protocol::MemberStatus {
        deployment_id: id.into(),
        generation,
        bundle_digest: "sha256:ab".into(),
        state: tensorplate_protocol::MemberState::Serving,
        admission_mode: admission,
        quota: tensorplate_protocol::MemberQuota::default(),
        unary_endpoint: None,
        stream_endpoint: None,
        stream_api_version: None,
        effective_quota: None,
        staged_bytes: None,
        contact: Some(contact),
    }
}

fn status_with_members(members: Vec<tensorplate_protocol::MemberStatus>) -> ControlResponse {
    ControlResponse {
        agent_status: Some(AgentStatus {
            agent_state: AgentRunState::Ready,
            resident_set: Some(ResidentSetStatus {
                set_id: "set-1".into(),
                revision: 4,
                members,
            }),
            ..AgentStatus::default()
        }),
        ..ControlResponse::ok(Some("c".into()))
    }
}

#[test]
fn status_member_lines_carry_every_field_and_contact_sets_severity() {
    use tensorplate_protocol::{AdmissionMode, ContactState};
    let mut unary = member(
        "vision-a",
        2,
        AdmissionMode::Production,
        ContactState::InContact,
    );
    unary.unary_endpoint = Some("http://127.0.0.1:18080".into());
    let mut stream = member(
        "speech-tts",
        5,
        AdmissionMode::Qualification,
        ContactState::OutOfContact,
    );
    stream.stream_endpoint = Some("127.0.0.1:18105".into());

    let stub = AgentStub::start();
    stub.enqueue(status_with_members(vec![unary.clone(), stream]));
    let (_code, stdout, _stderr) = run_cli(&stub.socket, &["status"]);
    assert!(
        stdout.contains("resident_set: set_id=set-1 revision=4 members=2"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "member: deployment_id=vision-a generation=2 state=serving admission=production sessions=0 unary=http://127.0.0.1:18080 contact=in_contact\n"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "member: deployment_id=speech-tts generation=5 state=serving admission=qualification (unqualified) sessions=0 stream=127.0.0.1:18105 contact=out_of_contact\n"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("severity=degraded"),
        "an out-of-contact member degrades severity: {stdout}"
    );

    let stub = AgentStub::start();
    stub.enqueue(status_with_members(vec![unary]));
    let (_code, stdout, _stderr) = run_cli(&stub.socket, &["status"]);
    assert!(stdout.contains("severity=ready"), "{stdout}");
}

#[test]
fn deploy_wait_finds_its_member_in_the_resident_set() {
    use tensorplate_protocol::deploy_transaction::DeployState;
    use tensorplate_protocol::{AdmissionMode, ContactState, DeployStatus};
    let stub = AgentStub::start();
    stub.enqueue(status_listing(&[FEATURE_SET_OPERATION_ADD]));
    stub.enqueue(ControlResponse {
        transaction_id: Some("tx-1".into()),
        deploy_status: Some(DeployStatus {
            phase: DeployState::Received,
            transaction_id: Some("tx-1".into()),
            deployment_id: Some("speech-tts".into()),
            bundle_digest: None,
            started_monotonic_ns: None,
            last_transition_monotonic_ns: None,
            failure: None,
        }),
        ..ControlResponse::ok(Some("c".into()))
    });
    // After commit: two members, so `active` is absent.
    stub.enqueue(status_with_members(vec![
        member(
            "speech-stt",
            3,
            AdmissionMode::Production,
            ContactState::InContact,
        ),
        member(
            "speech-tts",
            5,
            AdmissionMode::Production,
            ContactState::InContact,
        ),
    ]));
    let (_td, bundle) = write_bundle_dir();
    let (code, stdout, stderr) = run_cli(
        &stub.socket,
        &[
            "--output",
            "json",
            "deploy",
            bundle.to_str().unwrap(),
            "--deployment-id",
            "speech-tts",
            "--set-operation",
            "add",
            "--wait-timeout-ms",
            "5000",
        ],
    );
    assert_eq!(code, 0, "stderr was {stderr}");
    let body: Value = serde_json::from_str(&stdout).expect("json envelope");
    assert_eq!(body["payload"]["phase"], "active");
    assert_eq!(body["payload"]["deployment_id"], "speech-tts");
}

#[test]
fn infer_on_a_multi_member_set_asks_for_a_serving_url() {
    use tensorplate_protocol::{AdmissionMode, ContactState};
    let stub = AgentStub::start();
    stub.enqueue(status_with_members(vec![
        member(
            "speech-stt",
            3,
            AdmissionMode::Production,
            ContactState::InContact,
        ),
        member(
            "speech-tts",
            5,
            AdmissionMode::Production,
            ContactState::InContact,
        ),
    ]));
    let td = tempfile::TempDir::new().expect("tempdir");
    let input = td.path().join("request.json");
    std::fs::write(&input, b"{}").expect("write input");
    let (code, _stdout, stderr) =
        run_cli(&stub.socket, &["infer", "--input", input.to_str().unwrap()]);
    assert_eq!(code, 6, "stderr was {stderr}");
    assert!(stderr.contains("resident set of 2 members"), "{stderr}");
    assert!(stderr.contains("--serving-url"), "{stderr}");
}
