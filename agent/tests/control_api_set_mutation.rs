// SPDX-License-Identifier: Apache-2.0
//
// Set-mutation requests over the control socket, before the agent can
// execute any of them.
//
// `undeploy`, `recover`, `deploy` with `set_operation` `add`, qualification
// admission or an evidence reference, and a rollback naming a member are
// answered with a typed `unsupported` error before any transaction
// starts, so nothing is staged and the next request is not `busy`. A
// request carrying a field the agent does not know is refused at decode.
// With a committed resident set in the durable state, status projects it,
// a set of one serving member also fills the singleton `active`, and the
// deploy and rollback paths that would mutate the set are refused the same
// way, except that the two requests always refused (`replace` naming a
// non-member, and a rollback naming nobody, in a set of more than one
// member) are refused as invalid first, whatever else they carry.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access
)]

mod common;

use common::{vision_bundle, Harness};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use tensorplate_agent::server::Server;
use tensorplate_protocol::agent_control::{
    ControlRequest, ControlResponse, DeployRequest, MemberRequest, ResponseStatus, RollbackRequest,
    SetOperation, StatusRequest,
};
use tensorplate_protocol::agent_state::decode_agent_state;
use tensorplate_protocol::{AdmissionMode, ErrorCode};

fn exchange_raw(socket: &Path, line: &str) -> String {
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream
        .write_all(format!("{line}\n").as_bytes())
        .expect("write");
    stream.flush().expect("flush");
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).expect("read");
    response
}

fn exchange(socket: &Path, request: &ControlRequest) -> ControlResponse {
    let raw = exchange_raw(socket, &serde_json::to_string(request).expect("ser"));
    serde_json::from_str(&raw).expect("decode")
}

/// Assert a typed error with `code`, the request's correlation id, and a
/// message containing `says`.
fn assert_refused(response: &ControlResponse, code: ErrorCode, says: &str) {
    assert_eq!(response.status, ResponseStatus::Error, "{response:?}");
    let error = response.error.as_ref().expect("typed error");
    assert_eq!(error.code, code, "{response:?}");
    assert!(error.message.contains(says), "{}", error.message);
    assert_eq!(response.correlation_id.as_deref(), Some("ctl-1"));
}

/// Nothing started: no transaction in flight and nothing staged.
fn assert_untouched(h: &Harness) {
    let state = h.store.snapshot().expect("snapshot");
    assert!(
        state.in_flight_transaction.is_none(),
        "a transaction started"
    );
    let staged: Vec<PathBuf> = std::fs::read_dir(&h.config.staging_dir)
        .map(|entries| entries.map(|e| e.expect("entry").path()).collect())
        .unwrap_or_default();
    assert!(staged.is_empty(), "something was staged: {staged:?}");
}

fn deploy(bundle: &Path, deployment_id: &str) -> DeployRequest {
    DeployRequest {
        bundle_path: bundle.display().to_string(),
        deployment_id: deployment_id.into(),
        ..DeployRequest::default()
    }
}

fn member(deployment_id: &str) -> MemberRequest {
    MemberRequest {
        deployment_id: deployment_id.into(),
        reason: Some("operator request".into()),
    }
}

#[allow(clippy::unnecessary_wraps)] // the constructors take an `Option`
fn corr() -> Option<String> {
    Some("ctl-1".into())
}

/// Commit the resident set (and counter) a state fixture records.
fn commit_fixture_set(h: &Harness, fixture: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../protocol/rust/tests/fixtures")
        .join(fixture);
    let recorded = decode_agent_state(&std::fs::read_to_string(path).expect("read fixture"))
        .expect("fixture decodes");
    h.store
        .update(|state| {
            state.next_generation = recorded.next_generation;
            state.resident_set.clone_from(&recorded.resident_set);
            Ok(())
        })
        .expect("commit the set");
}

#[test]
fn undeploy_and_recover_are_typed_unsupported() {
    let h = Harness::new();
    let socket = h.config.socket_path.clone().expect("socket");
    let mut server = Server::start(&h.config, h.coord.clone()).expect("start");
    let undeploy = exchange(&socket, &ControlRequest::undeploy(corr(), member("d1")));
    assert_refused(
        &undeploy,
        ErrorCode::Unsupported,
        "`undeploy` is not supported by this agent yet",
    );
    let recover = exchange(&socket, &ControlRequest::recover(corr(), member("d1")));
    assert_refused(
        &recover,
        ErrorCode::Unsupported,
        "`recover` is not supported by this agent yet",
    );
    assert_untouched(&h);
    server.shutdown();
}

#[test]
fn set_mutation_deploys_are_refused_before_any_transaction() {
    let h = Harness::new();
    let socket = h.config.socket_path.clone().expect("socket");
    let mut server = Server::start(&h.config, h.coord.clone()).expect("start");
    let bundle = vision_bundle(h.td.path(), "d1");
    let cases = [
        (
            DeployRequest {
                set_operation: SetOperation::Add,
                ..deploy(&bundle, "d1")
            },
            "set_operation `add`",
        ),
        (
            DeployRequest {
                admission_mode: Some(AdmissionMode::Qualification),
                test_count: Some(2),
                ..deploy(&bundle, "d1")
            },
            "qualification admission",
        ),
        (
            DeployRequest {
                evidence_ref: Some("evidence-7".into()),
                ..deploy(&bundle, "d1")
            },
            "evidence_ref",
        ),
    ];
    for (payload, says) in cases {
        let response = exchange(&socket, &ControlRequest::deploy(corr(), payload));
        assert_refused(&response, ErrorCode::Unsupported, says);
        assert_untouched(&h);
    }
    // Nothing was left in flight: a default deploy and an explicit
    // `production` one go through.
    let ok = exchange(
        &socket,
        &ControlRequest::deploy(
            None,
            DeployRequest {
                admission_mode: Some(AdmissionMode::Production),
                ..deploy(&bundle, "d1")
            },
        ),
    );
    assert_eq!(ok.status, ResponseStatus::Ok, "{ok:?}");
    server.shutdown();
}

#[test]
fn a_rollback_naming_a_member_is_unsupported() {
    let h = Harness::new();
    let socket = h.config.socket_path.clone().expect("socket");
    let mut server = Server::start(&h.config, h.coord.clone()).expect("start");
    let response = exchange(
        &socket,
        &ControlRequest::rollback(
            corr(),
            RollbackRequest {
                reason: None,
                deployment_id: Some("d1".into()),
            },
        ),
    );
    assert_refused(
        &response,
        ErrorCode::Unsupported,
        "`rollback` of a named member",
    );
    assert_untouched(&h);
    server.shutdown();
}

#[test]
fn unknown_and_misplaced_request_fields_are_refused_at_decode() {
    let h = Harness::new();
    let socket = h.config.socket_path.clone().expect("socket");
    let mut server = Server::start(&h.config, h.coord.clone()).expect("start");
    let bundle = vision_bundle(h.td.path(), "d1").display().to_string();
    let lines = [
        // An operator-only field outside the deploy payload.
        format!(
            r#"{{"schema_version":"0.1","op":"deploy","admission_mode":"qualification","deploy":{{"bundle_path":"{bundle}","deployment_id":"d1"}}}}"#
        ),
        // Operator-only fields on the status query the SDK sends.
        r#"{"schema_version":"0.1","op":"status","status":{"include_quarantine":true,"test_count":2}}"#.to_string(),
        // A deploy field this agent does not know.
        format!(
            r#"{{"schema_version":"0.1","op":"deploy","deploy":{{"bundle_path":"{bundle}","deployment_id":"d1","priority":1}}}}"#
        ),
    ];
    for line in lines {
        let response: ControlResponse =
            serde_json::from_str(&exchange_raw(&socket, &line)).expect("decode");
        assert_eq!(response.status, ResponseStatus::Error, "{line}");
        let error = response.error.expect("typed error");
        assert_eq!(error.code, ErrorCode::ConfigInvalid, "{line}");
        assert!(error.message.contains("unknown field"), "{}", error.message);
    }
    assert_untouched(&h);
    server.shutdown();
}

#[test]
fn a_committed_two_member_set_is_projected() {
    let h = Harness::new();
    commit_fixture_set(&h, "agent_state_0_2_two_member_set.json");
    let socket = h.config.socket_path.clone().expect("socket");
    let mut server = Server::start(&h.config, h.coord.clone()).expect("start");

    let raw = exchange_raw(
        &socket,
        &serde_json::to_string(&ControlRequest::status(corr(), StatusRequest::default()))
            .expect("ser"),
    );
    let value: Value = serde_json::from_str(&raw).expect("json");
    let schema_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../protocol/schemas/agent_control.json");
    let schema: Value =
        serde_json::from_str(&std::fs::read_to_string(schema_path).expect("schema"))
            .expect("schema parses");
    let validator = jsonschema::JSONSchema::compile(&schema).expect("compiles");
    assert!(validator.is_valid(&value), "status is schema-valid: {raw}");
    let status: ControlResponse = serde_json::from_str(&raw).expect("decode");
    let agent = status.agent_status.expect("agent status");
    let set = agent.resident_set.expect("resident set");
    let members: Vec<(&str, u64)> = set
        .members
        .iter()
        .map(|m| (m.deployment_id.as_str(), m.generation))
        .collect();
    assert_eq!(members, [("speech-stt", 3), ("speech-tts", 5)]);
    assert!(
        agent.active.is_none(),
        "a two-member set fills no singleton"
    );
    assert!(agent.previous_active.is_none());
    assert!(agent.control_features.is_empty());
    server.shutdown();
}

#[test]
fn a_committed_two_member_set_guards_its_mutations() {
    let h = Harness::new();
    commit_fixture_set(&h, "agent_state_0_2_two_member_set.json");
    let socket = h.config.socket_path.clone().expect("socket");
    let mut server = Server::start(&h.config, h.coord.clone()).expect("start");
    let bundle = vision_bundle(h.td.path(), "vision-new");
    let non_member = exchange(
        &socket,
        &ControlRequest::deploy(corr(), deploy(&bundle, "vision-new")),
    );
    assert_refused(
        &non_member,
        ErrorCode::ConfigInvalid,
        "`vision-new` is not a member of the resident set",
    );
    // Invalid first, even when an unsupported field rides along.
    let non_member_with_evidence = exchange(
        &socket,
        &ControlRequest::deploy(
            corr(),
            DeployRequest {
                evidence_ref: Some("evidence-7".into()),
                ..deploy(&bundle, "vision-new")
            },
        ),
    );
    assert_refused(
        &non_member_with_evidence,
        ErrorCode::ConfigInvalid,
        "`vision-new` is not a member of the resident set",
    );
    let add_non_member = exchange(
        &socket,
        &ControlRequest::deploy(
            corr(),
            DeployRequest {
                set_operation: SetOperation::Add,
                ..deploy(&bundle, "vision-new")
            },
        ),
    );
    assert_refused(
        &add_non_member,
        ErrorCode::Unsupported,
        "set_operation `add`",
    );
    let replace = exchange(
        &socket,
        &ControlRequest::deploy(corr(), deploy(&bundle, "speech-tts")),
    );
    assert_refused(
        &replace,
        ErrorCode::Unsupported,
        "`deploy` into a resident set",
    );
    let unnamed = exchange(
        &socket,
        &ControlRequest::rollback(corr(), RollbackRequest::default()),
    );
    assert_refused(
        &unnamed,
        ErrorCode::ConfigInvalid,
        "must name a deployment_id when the resident set has more than one member",
    );
    let named = exchange(
        &socket,
        &ControlRequest::rollback(
            corr(),
            RollbackRequest {
                reason: None,
                deployment_id: Some("speech-tts".into()),
            },
        ),
    );
    assert_refused(
        &named,
        ErrorCode::Unsupported,
        "`rollback` of a named member",
    );
    assert_untouched(&h);
    server.shutdown();
}

#[test]
fn a_committed_size_one_set_fills_the_singleton_slots() {
    let h = Harness::new();
    commit_fixture_set(&h, "agent_state_0_2_restore_step.json");
    let socket = h.config.socket_path.clone().expect("socket");
    let mut server = Server::start(&h.config, h.coord.clone()).expect("start");

    let status = exchange(
        &socket,
        &ControlRequest::status(corr(), StatusRequest::default()),
    );
    let agent = status.agent_status.expect("agent status");
    let active = agent.active.expect("the only serving member is active");
    assert_eq!(active.deployment_id, "vision-v2");
    assert_eq!(
        active.serving_url.as_deref(),
        Some("http://127.0.0.1:18080/infer")
    );
    assert_eq!(
        agent.previous_active.expect("retained").deployment_id,
        "vision-v1"
    );
    assert_eq!(agent.resident_set.expect("set").members.len(), 1);

    // Deploying or rolling back would mutate the set: refused, not stuck.
    let bundle = vision_bundle(h.td.path(), "vision-v3");
    let replace = exchange(
        &socket,
        &ControlRequest::deploy(corr(), deploy(&bundle, "vision-v3")),
    );
    assert_refused(
        &replace,
        ErrorCode::Unsupported,
        "`deploy` into a resident set",
    );
    let rollback = exchange(
        &socket,
        &ControlRequest::rollback(corr(), RollbackRequest::default()),
    );
    assert_refused(
        &rollback,
        ErrorCode::Unsupported,
        "`rollback` in a resident set",
    );
    assert_untouched(&h);
    server.shutdown();
}
