// SPDX-License-Identifier: Apache-2.0
//
// `tensorplate status`, and `tensorplate doctor`'s finding on the members of
// a set, against the status responses recorded from an agent
// that runs the member registry
// (`protocol/rust/tests/fixtures/agent_control_member_status.jsonl`), which
// carry the serving generation and the state of a member's worker process,
// and against the status recorded before those fields existed.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use common::{run_cli, AgentStub};
use serde_json::Value;
use tensorplate_protocol::agent_control::{ContactState, ControlResponse, ProcessState, Readiness};
use tensorplate_protocol::decode_with_version_check;

/// The response on line `line` (from 1) of a recorded exchange.
fn recorded(fixture: &str, line: usize) -> ControlResponse {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../protocol/rust/tests/fixtures")
        .join(fixture);
    let text = std::fs::read_to_string(path).expect("fixture");
    let line = text.lines().nth(line - 1).expect("the line is recorded");
    let response: ControlResponse = decode_with_version_check(line).expect("a response");
    assert!(response.agent_status.is_some(), "a status response");
    response
}

/// `status` in human and in JSON form against an agent answering `response`.
fn status_of(response: &ControlResponse) -> (String, Value) {
    let stub = AgentStub::start();
    stub.enqueue(response.clone());
    stub.enqueue(response.clone());
    let (code, human, stderr) = run_cli(&stub.socket, &["status"]);
    assert_eq!(code, 0, "stderr was {stderr}");
    let (code, json, stderr) = run_cli(&stub.socket, &["--output", "json", "status"]);
    assert_eq!(code, 0, "stderr was {stderr}");
    (human, serde_json::from_str(&json).expect("json envelope"))
}

/// What `doctor` says of the members of the set in `response`.
fn doctor_on_members(response: &ControlResponse) -> Value {
    let stub = AgentStub::start();
    // doctor sends: version, then status.
    stub.enqueue(ControlResponse::ok(Some("v".into())));
    stub.enqueue(response.clone());
    let (code, stdout, stderr) = run_cli(&stub.socket, &["--output", "json", "doctor"]);
    assert_eq!(code, 0, "stderr was {stderr}");
    let body: Value = serde_json::from_str(&stdout).expect("json envelope");
    let findings = body["payload"]["findings"].as_array().expect("findings");
    let members = findings.iter().find(|f| f["id"] == "resident_set_members");
    members.expect("a finding on the members").clone()
}

#[test]
fn status_names_the_generation_serving_a_singleton_deployment() {
    let (human, json) = status_of(&recorded("agent_control_member_status.jsonl", 2));

    assert!(
        human.contains(
            "active: deployment_id=vision-v2 backend=mock bundle=bundle-vision-v2@vision-v2\n  generation=2\n  serving_url=http://127.0.0.1:41281/infer\n"
        ),
        "{human}"
    );
    let agent = &json["payload"]["agent"];
    assert_eq!(agent["active"]["generation"], 2);
    assert_eq!(
        agent["active"]["serving_url"],
        "http://127.0.0.1:41281/infer"
    );
    assert!(
        agent["previous_active"]["generation"].is_null(),
        "nothing names the generation of the deployment before it"
    );
}

#[test]
fn status_reports_the_process_of_a_committed_member() {
    let (human, json) = status_of(&recorded("agent_control_member_status.jsonl", 4));

    assert!(
        human.contains(
            "  member: deployment_id=vision-v2 generation=2 state=serving admission=production sessions=0 unary=http://127.0.0.1:41281 contact=in_contact process=running readiness=ready draining=false\n"
        ),
        "{human}"
    );
    assert!(human.contains("\n  generation=2\n"), "{human}");
    let agent = &json["payload"]["agent"];
    let member = &agent["resident_set"]["members"][0];
    assert_eq!(member["process"], "running");
    assert_eq!(member["readiness"], "ready");
    assert_eq!(member["draining"], false);
    assert_eq!(member["contact"], "in_contact");
    assert_eq!(agent["active"]["generation"], 2);
    assert_eq!(agent["previous_active"]["generation"], 1);
}

#[test]
fn doctor_says_which_members_are_in_contact_and_which_have_no_worker_in_service() {
    let mut response = recorded("agent_control_member_status.jsonl", 4);
    let judged = doctor_on_members(&response);
    assert_eq!(judged["status"], "ok", "{judged}");
    assert_eq!(
        judged["message"],
        "1 member in `serving` state and in contact"
    );

    let set = response
        .agent_status
        .as_mut()
        .and_then(|status| status.resident_set.as_mut())
        .expect("a set");
    let mut stopped = set.members[0].clone();
    stopped.deployment_id = "vision-v3".into();
    stopped.process = Some(ProcessState::Stopped);
    stopped.readiness = Some(Readiness::NotReady);
    stopped.contact = None;
    set.members.push(stopped);
    let judged = doctor_on_members(&response);

    // Not judged: a stopped worker does not change the verdict, only the message.
    assert_eq!(judged["status"], "ok", "{judged}");
    assert_eq!(
        judged["message"],
        "2 members in `serving` state; 1 in contact, 1 with a worker that is stopped or draining"
    );
}

#[test]
fn status_reports_a_member_that_is_stopped_and_one_that_is_draining() {
    let mut response = recorded("agent_control_member_status.jsonl", 4);
    let set = response
        .agent_status
        .as_mut()
        .and_then(|status| status.resident_set.as_mut())
        .expect("a set");
    let mut draining = set.members[0].clone();
    draining.deployment_id = "vision-v3".into();
    draining.contact = None;
    draining.draining = Some(true);
    let stopped = &mut set.members[0];
    stopped.process = Some(ProcessState::Stopped);
    stopped.readiness = Some(Readiness::NotReady);
    stopped.contact = None;
    set.members.push(draining);

    let (human, json) = status_of(&response);

    assert!(
        human.contains(
            "unary=http://127.0.0.1:41281 process=stopped readiness=not_ready draining=false\n"
        ),
        "{human}"
    );
    assert!(
        human.contains(
            "deployment_id=vision-v3 generation=2 state=serving admission=production sessions=0 unary=http://127.0.0.1:41281 process=running readiness=ready draining=true\n"
        ),
        "{human}"
    );
    let members = &json["payload"]["agent"]["resident_set"]["members"];
    assert_eq!(members[0]["process"], "stopped");
    assert_eq!(members[0]["readiness"], "not_ready");
    assert_eq!(members[1]["draining"], true);
    assert!(members[1].get("contact").is_none());
    // Severity judges `contact`, and neither member reports one.
    assert_eq!(json["payload"]["severity"], "ready");
}

#[test]
fn a_member_the_agent_reports_out_of_contact_lowers_severity() {
    let mut response = recorded("agent_control_member_status.jsonl", 4);
    let (_, json) = status_of(&response);
    assert_eq!(json["payload"]["severity"], "ready");

    let set = response
        .agent_status
        .as_mut()
        .and_then(|status| status.resident_set.as_mut())
        .expect("a set");
    set.members[0].contact = Some(ContactState::OutOfContact);
    let (human, json) = status_of(&response);

    assert!(
        human.contains("contact=out_of_contact process=running"),
        "{human}"
    );
    assert_eq!(json["payload"]["severity"], "degraded");
}

#[test]
fn a_status_recorded_before_the_fields_existed_renders_without_them() {
    let (human, json) = status_of(&recorded("agent_control_singleton_lifecycle.jsonl", 6));

    assert!(
        human.contains("active: deployment_id=d2 backend=mock bundle="),
        "{human}"
    );
    for absent in ["generation=", "process=", "readiness=", "draining="] {
        assert!(!human.contains(absent), "`{absent}` in {human}");
    }
    let active = &json["payload"]["agent"]["active"];
    assert_eq!(active["deployment_id"], "d2");
    assert!(active["generation"].is_null());
    assert!(active
        .as_object()
        .expect("active")
        .contains_key("generation"));
}
