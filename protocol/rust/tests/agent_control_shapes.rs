// SPDX-License-Identifier: Apache-2.0
//
// The agent control API's set-mutation shapes and resident-set status.
//
// `protocol/schemas/agent_control.json` and the Rust mirror in
// `agent_control.rs` change together. This suite holds them to each other:
// the recorded singleton exchange still decodes and re-encodes byte for
// byte and the constructors still build its requests; the schema's enums
// and shared definitions match the Rust enums and the state schema; each
// single-fault request is refused by both readers, by the schema at exactly
// its named location and by the decoder with its own message; and the
// resident-set status projection is checked against the committed state
// fixtures.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::{json, Value};
use tensorplate_protocol::agent_control::{
    singleton_slots, ControlOp, ControlRequest, ControlResponse, DeployRequest, MemberRequest,
    ResidentSetStatus, RollbackRequest, SetOperation, StatusRequest,
};
use tensorplate_protocol::agent_state::decode_agent_state;
use tensorplate_protocol::resident_set::{MemberState, ResidentSet};
use tensorplate_protocol::{
    decode_with_version_check, AdmissionMode, ContactState, DecodeError, FEATURE_MEMBER_ROLLBACK,
    FEATURE_SET_OPERATION_ADD,
};

const GOLDEN: &str = "agent_control_singleton_lifecycle.jsonl";

fn fixture(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
}

fn schema_file(name: &str) -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../schemas")
        .join(name);
    let raw =
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read schema {}: {e}", p.display()));
    serde_json::from_str(&raw).expect("schema parses")
}

fn control_schema() -> Value {
    schema_file("agent_control.json")
}

fn validator() -> &'static jsonschema::JSONSchema {
    static VALIDATOR: std::sync::OnceLock<jsonschema::JSONSchema> = std::sync::OnceLock::new();
    VALIDATOR.get_or_init(|| {
        jsonschema::JSONSchema::compile(&control_schema()).expect("schema compiles as Draft-07")
    })
}

/// A validator rooted at one envelope definition, so its errors name the
/// failing location; through the whole schema's root `oneOf` every failure
/// is reported at the root.
fn envelope_validator(definition: &str) -> jsonschema::JSONSchema {
    let schema = control_schema();
    let mut root = schema["definitions"][definition].clone();
    let object = root.as_object_mut().expect("definition");
    object.insert("$schema".into(), schema["$schema"].clone());
    object.insert("definitions".into(), schema["definitions"].clone());
    jsonschema::JSONSchema::compile(&root).expect("envelope compiles as Draft-07")
}

fn request_validator() -> &'static jsonschema::JSONSchema {
    static VALIDATOR: std::sync::OnceLock<jsonschema::JSONSchema> = std::sync::OnceLock::new();
    VALIDATOR.get_or_init(|| envelope_validator("Request"))
}

fn response_validator() -> &'static jsonschema::JSONSchema {
    static VALIDATOR: std::sync::OnceLock<jsonschema::JSONSchema> = std::sync::OnceLock::new();
    VALIDATOR.get_or_init(|| envelope_validator("Response"))
}

fn is_response(frame: &Value) -> bool {
    frame.get("status").is_some_and(Value::is_string)
}

/// Where the frame's own envelope reports errors; empty when the whole
/// schema accepts it. The envelope and the whole schema must agree.
fn schema_error_paths(instance: &Value) -> BTreeSet<String> {
    let envelope = if is_response(instance) {
        response_validator()
    } else {
        request_validator()
    };
    let paths: BTreeSet<String> = match envelope.validate(instance) {
        Ok(()) => BTreeSet::new(),
        Err(errors) => errors.map(|e| e.instance_path.to_string()).collect(),
    };
    assert_eq!(
        paths.is_empty(),
        validator().is_valid(instance),
        "the envelope and the whole schema disagree on {instance}"
    );
    paths
}

fn decode_request(instance: &Value) -> Result<ControlRequest, DecodeError> {
    decode_with_version_check(&serde_json::to_string(instance).expect("encode"))
}

fn committed_set(fixture_name: &str) -> ResidentSet {
    decode_agent_state(&fixture(fixture_name))
        .expect("state fixture decodes")
        .resident_set
        .expect("fixture records a set")
}

// ---- The recorded singleton exchange ----------------------------------------

#[test]
fn the_recorded_exchange_still_decodes_and_reencodes_byte_for_byte() {
    for (index, line) in fixture(GOLDEN).lines().enumerate() {
        let value: Value = serde_json::from_str(line).expect("json");
        let errors = schema_error_paths(&value);
        assert!(
            errors.is_empty(),
            "line {}: schema errors at {errors:?}",
            index + 1
        );
        let reencoded = if is_response(&value) {
            let response: ControlResponse =
                decode_with_version_check(line).expect("response decodes");
            serde_json::to_string(&response).expect("encode")
        } else {
            let request: ControlRequest = decode_with_version_check(line).expect("request decodes");
            serde_json::to_string(&request).expect("encode")
        };
        assert_eq!(reencoded, line, "line {} changes on re-encode", index + 1);
    }
}

#[test]
fn a_default_deploy_and_rollback_build_the_recorded_requests() {
    let golden: Vec<String> = fixture(GOLDEN).lines().map(str::to_string).collect();
    let mut labels = BTreeMap::new();
    labels.insert("team".to_string(), "vision".to_string());
    let deploy = ControlRequest::deploy(
        Some("ctl-000001".into()),
        DeployRequest {
            bundle_path: "/srv/golden/bundle-d1".into(),
            deployment_id: "d1".into(),
            labels,
            set_operation: SetOperation::Replace,
            ..DeployRequest::default()
        },
    );
    assert_eq!(serde_json::to_string(&deploy).expect("encode"), golden[0]);
    let status = ControlRequest::status(Some("ctl-000003".into()), StatusRequest::default());
    assert_eq!(serde_json::to_string(&status).expect("encode"), golden[4]);
    let rollback = ControlRequest::rollback(
        Some("ctl-000004".into()),
        RollbackRequest {
            reason: Some("operator rollback".into()),
            deployment_id: None,
        },
    );
    assert_eq!(serde_json::to_string(&rollback).expect("encode"), golden[6]);
}

// ---- Drift between the schema, the Rust mirror and the state schema ----------

fn spelling<T: serde::Serialize>(value: T) -> Value {
    serde_json::to_value(value).expect("spelling")
}

/// Every operation. Adding a `ControlOp` variant fails to compile in the
/// match until it is handled; the list beside it must gain the variant too,
/// and the schema comparison fails until the schema does.
fn all_ops() -> Vec<ControlOp> {
    let all = vec![
        ControlOp::Deploy,
        ControlOp::Status,
        ControlOp::Rollback,
        ControlOp::Health,
        ControlOp::Version,
        ControlOp::Undeploy,
        ControlOp::Recover,
    ];
    for op in &all {
        match op {
            ControlOp::Deploy
            | ControlOp::Status
            | ControlOp::Rollback
            | ControlOp::Health
            | ControlOp::Version
            | ControlOp::Undeploy
            | ControlOp::Recover => {}
        }
    }
    all
}

#[test]
fn schema_enums_match_the_mirror() {
    let schema = control_schema();
    let defs = &schema["definitions"];
    let ops: Vec<Value> = all_ops().into_iter().map(spelling).collect();
    assert_eq!(
        defs["Request"]["properties"]["op"]["enum"],
        Value::Array(ops)
    );
    for op in all_ops() {
        assert_eq!(spelling(op), json!(op.to_string()), "ControlOp's Display");
    }
    let set_operations = [SetOperation::Replace, SetOperation::Add];
    for op in set_operations {
        match op {
            SetOperation::Replace | SetOperation::Add => {}
        }
    }
    assert_eq!(
        defs["DeployRequest"]["properties"]["set_operation"]["enum"],
        Value::Array(set_operations.into_iter().map(spelling).collect())
    );
    let modes = [AdmissionMode::Production, AdmissionMode::Qualification];
    for mode in modes {
        match mode {
            AdmissionMode::Production | AdmissionMode::Qualification => {}
        }
    }
    let modes = Value::Array(modes.into_iter().map(spelling).collect());
    assert_eq!(
        defs["DeployRequest"]["properties"]["admission_mode"]["enum"],
        modes
    );
    assert_eq!(
        defs["MemberStatus"]["properties"]["admission_mode"]["enum"],
        modes
    );
    let states = [MemberState::Serving, MemberState::Quarantined];
    for state in states {
        match state {
            MemberState::Serving | MemberState::Quarantined => {}
        }
    }
    assert_eq!(
        defs["MemberStatus"]["properties"]["state"]["enum"],
        Value::Array(states.into_iter().map(spelling).collect())
    );
    let contact = [ContactState::InContact, ContactState::OutOfContact];
    for state in contact {
        match state {
            ContactState::InContact | ContactState::OutOfContact => {}
        }
    }
    assert_eq!(
        defs["MemberStatus"]["properties"]["contact"]["enum"],
        Value::Array(contact.into_iter().map(spelling).collect())
    );
}

#[test]
fn the_shared_definitions_match_the_state_schema() {
    let control = control_schema();
    let state = schema_file("agent_state.json");
    for definition in [
        "MemberQuota",
        "DomainQuotaBytes",
        "QuotaBytes",
        "StateCounter",
        "Digest",
    ] {
        assert_eq!(
            control["definitions"][definition], state["definitions"][definition],
            "{definition} differs between agent_control.json and agent_state.json"
        );
    }
}

#[test]
fn the_control_features_are_spelled_as_the_schema_allows() {
    let schema = control_schema();
    let description = schema["definitions"]["AgentStatus"]["properties"]["control_features"]
        ["description"]
        .as_str()
        .expect("description");
    for feature in [FEATURE_SET_OPERATION_ADD, FEATURE_MEMBER_ROLLBACK] {
        assert!(
            description.contains(&format!("`{feature}`")),
            "{feature} undocumented"
        );
        let status = json!({
            "schema_version": "0.1", "status": "ok",
            "agent_status": {"agent_state": "ready", "control_features": [feature]}
        });
        assert!(schema_error_paths(&status).is_empty(), "{feature}");
    }
}

// ---- Verdicts: schema and decoder together ----------------------------------

type Edit = fn(&mut Value);

struct Refusal {
    label: &'static str,
    edit: Edit,
    schema_at: &'static str,
    decoder: &'static str,
}

const fn refusal(
    label: &'static str,
    edit: Edit,
    schema_at: &'static str,
    decoder: &'static str,
) -> Refusal {
    Refusal {
        label,
        edit,
        schema_at,
        decoder,
    }
}

fn deploy_base() -> Value {
    json!({
        "schema_version": "0.1", "correlation_id": "ctl-1", "op": "deploy",
        "deploy": {"bundle_path": "/srv/bundle", "deployment_id": "speech-tts"}
    })
}

fn remove(v: &mut Value, key: &str) {
    v.as_object_mut().expect("object").remove(key);
}

/// Requests refused by both readers, each for one reason. Every case edits
/// a valid default deploy.
#[allow(clippy::too_many_lines)]
fn refused_requests() -> Vec<Refusal> {
    vec![
        refusal(
            "an unknown request field",
            |v| v["later"] = json!(1),
            "",
            "unknown field `later`",
        ),
        refusal(
            "an operator-only field outside the deploy payload",
            |v| v["admission_mode"] = json!("qualification"),
            "",
            "unknown field `admission_mode`",
        ),
        refusal(
            "an unknown deploy field",
            |v| v["deploy"]["priority"] = json!(1),
            "/deploy",
            "unknown field `priority`",
        ),
        refusal(
            "an unknown op",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("reload");
            },
            "/op",
            "unknown variant `reload`",
        ),
        refusal(
            "a deploy without its payload",
            |v| remove(v, "deploy"),
            "",
            "`deploy` operation requires a `deploy` payload",
        ),
        refusal(
            "a status payload on a deploy",
            |v| v["status"] = json!({"include_quarantine": true}),
            "/op",
            "`deploy` operation must not carry a `status` payload",
        ),
        refusal(
            "a deploy payload on a status query",
            |v| v["op"] = json!("status"),
            "/op",
            "`status` operation must not carry a `deploy` payload",
        ),
        refusal(
            "a rollback payload on a health probe",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("health");
                v["rollback"] = json!({});
            },
            "/op",
            "`health` operation must not carry a `rollback` payload",
        ),
        refusal(
            "an undeploy without its payload",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("undeploy");
            },
            "",
            "`undeploy` operation requires a `undeploy` payload",
        ),
        refusal(
            "a recover without its payload",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("recover");
            },
            "",
            "`recover` operation requires a `recover` payload",
        ),
        refusal(
            "an undeploy payload on a recover",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("recover");
                v["recover"] = json!({"deployment_id": "speech-tts"});
                v["undeploy"] = json!({"deployment_id": "speech-tts"});
            },
            "/op",
            "`recover` operation must not carry a `undeploy` payload",
        ),
        refusal(
            "a member payload without its member",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("undeploy");
                v["undeploy"] = json!({"reason": "retired"});
            },
            "/undeploy",
            "missing field `deployment_id`",
        ),
        refusal(
            "a recover payload on an undeploy",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("undeploy");
                v["undeploy"] = json!({"deployment_id": "speech-tts"});
                v["recover"] = json!({"deployment_id": "speech-tts"});
            },
            "/op",
            "`undeploy` operation must not carry a `recover` payload",
        ),
        refusal(
            "an undeploy naming an unsafe member",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("undeploy");
                v["undeploy"] = json!({"deployment_id": ".."});
            },
            "/undeploy/deployment_id",
            "undeploy.deployment_id must be 1 to 128 bytes",
        ),
        refusal(
            "an unknown member-request field",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("recover");
                v["recover"] = json!({"deployment_id": "speech-tts", "generation": 3});
            },
            "/recover",
            "unknown field `generation`",
        ),
        refusal(
            "a null member reason",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("recover");
                v["recover"] = json!({"deployment_id": "speech-tts", "reason": null});
            },
            "/recover/reason",
            "invalid type: null",
        ),
        refusal(
            "a rollback naming an unsafe member",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("rollback");
                v["rollback"] = json!({"deployment_id": "speech/tts"});
            },
            "/rollback/deployment_id",
            "rollback.deployment_id must be 1 to 128 bytes",
        ),
        refusal(
            "an unknown set operation",
            |v| v["deploy"]["set_operation"] = json!("merge"),
            "/deploy/set_operation",
            "unknown variant `merge`",
        ),
        refusal(
            "a null admission mode",
            |v| v["deploy"]["admission_mode"] = Value::Null,
            "/deploy/admission_mode",
            "invalid type: null",
        ),
        refusal(
            "qualification without a test count",
            |v| v["deploy"]["admission_mode"] = json!("qualification"),
            "/deploy",
            "requires a test_count",
        ),
        refusal(
            "a test count without qualification",
            |v| v["deploy"]["test_count"] = json!(2),
            "/deploy",
            "test_count requires admission_mode `qualification`",
        ),
        refusal(
            "a test count beside production admission",
            |v| {
                v["deploy"]["admission_mode"] = json!("production");
                v["deploy"]["test_count"] = json!(2);
            },
            "/deploy",
            "test_count requires admission_mode `qualification`",
        ),
        refusal(
            "a zero test count",
            |v| {
                v["deploy"]["admission_mode"] = json!("qualification");
                v["deploy"]["test_count"] = json!(0);
            },
            "/deploy/test_count",
            "test_count must be at least 1",
        ),
        refusal(
            "an evidence reference in qualification mode",
            |v| {
                v["deploy"]["admission_mode"] = json!("qualification");
                v["deploy"]["test_count"] = json!(1);
                v["deploy"]["evidence_ref"] = json!("qual-run-7");
            },
            "/deploy",
            "does not belong with admission_mode `qualification`",
        ),
        refusal(
            "an empty evidence reference",
            |v| v["deploy"]["evidence_ref"] = json!(""),
            "/deploy/evidence_ref",
            "evidence_ref must be 1 to 256 characters",
        ),
        refusal(
            "an evidence reference with a control character",
            |v| v["deploy"]["evidence_ref"] = json!("run\n7"),
            "/deploy/evidence_ref",
            "evidence_ref must be 1 to 256 characters",
        ),
        refusal(
            "an evidence reference past the bound",
            |v| v["deploy"]["evidence_ref"] = json!("e".repeat(257)),
            "/deploy/evidence_ref",
            "evidence_ref must be 1 to 256 characters",
        ),
        refusal(
            "a null correlation id",
            |v| v["correlation_id"] = Value::Null,
            "/correlation_id",
            "invalid type: null",
        ),
        refusal(
            "a null expected digest",
            |v| v["deploy"]["expected_bundle_digest"] = Value::Null,
            "/deploy/expected_bundle_digest",
            "invalid type: null",
        ),
        refusal(
            "a deploy payload as an array",
            |v| v["deploy"] = json!(["/srv/bundle", "speech-tts"]),
            "/deploy",
            "invalid type: sequence",
        ),
        refusal(
            "an evidence reference past the bound in multi-byte characters",
            |v| v["deploy"]["evidence_ref"] = json!("\u{e9}".repeat(257)),
            "/deploy/evidence_ref",
            "evidence_ref must be 1 to 256 characters",
        ),
        refusal(
            "a null test count",
            |v| {
                v["deploy"]["admission_mode"] = json!("qualification");
                v["deploy"]["test_count"] = Value::Null;
            },
            "/deploy/test_count",
            "invalid type: null",
        ),
        refusal(
            "a null evidence reference",
            |v| v["deploy"]["evidence_ref"] = Value::Null,
            "/deploy/evidence_ref",
            "invalid type: null",
        ),
        refusal(
            "an unknown rollback field",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("rollback");
                v["rollback"] = json!({"deployment": "speech-tts"});
            },
            "/rollback",
            "unknown field `deployment`",
        ),
        refusal(
            "a rollback payload as an array",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("rollback");
                v["rollback"] = json!(["bad release"]);
            },
            "/rollback",
            "invalid type: sequence",
        ),
        refusal(
            "a null rollback reason",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("rollback");
                v["rollback"] = json!({"reason": null});
            },
            "/rollback/reason",
            "invalid type: null",
        ),
        refusal(
            "a null rollback member",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("rollback");
                v["rollback"] = json!({"deployment_id": null});
            },
            "/rollback/deployment_id",
            "invalid type: null",
        ),
        refusal(
            "an undeploy payload as an array",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("undeploy");
                v["undeploy"] = json!(["speech-tts"]);
            },
            "/undeploy",
            "invalid type: sequence",
        ),
        refusal(
            "a recover payload as an array",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("recover");
                v["recover"] = json!(["speech-tts"]);
            },
            "/recover",
            "invalid type: sequence",
        ),
        refusal(
            "an unknown undeploy field",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("undeploy");
                v["undeploy"] = json!({"deployment_id": "speech-tts", "drain_ms": 1});
            },
            "/undeploy",
            "unknown field `drain_ms`",
        ),
        refusal(
            "an operator-only field in the status payload",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("status");
                v["status"] = json!({"include_quarantine": true, "test_count": 2});
            },
            "/status",
            "unknown field `test_count`",
        ),
        refusal(
            "a null status payload",
            |v| {
                remove(v, "deploy");
                v["op"] = json!("status");
                v["status"] = Value::Null;
            },
            "/status",
            "invalid type: null",
        ),
    ]
}

#[test]
fn requests_both_readers_refuse() {
    for Refusal {
        label,
        edit,
        schema_at,
        decoder,
    } in refused_requests()
    {
        let mut request = deploy_base();
        edit(&mut request);
        // Exactly one location: a case that breaks two rules could pass for
        // the one it does not name.
        assert_eq!(
            schema_error_paths(&request),
            BTreeSet::from([schema_at.to_string()]),
            "{label}: the schema's errors are not all at `{schema_at}`"
        );
        match decode_request(&request) {
            Ok(_) => panic!("{label}: the decoder accepted it"),
            Err(err) => assert!(
                err.to_string().contains(decoder),
                "{label}: expected the decoder to say `{decoder}`, got `{err}`"
            ),
        }
    }
}

#[test]
fn requests_both_readers_accept() {
    let cases: Vec<(&str, Edit)> = vec![
        ("the default deploy", |_| {}),
        ("an explicit replace", |v| {
            v["deploy"]["set_operation"] = json!("replace");
        }),
        ("an add", |v| v["deploy"]["set_operation"] = json!("add")),
        ("qualification with a test count", |v| {
            v["deploy"]["admission_mode"] = json!("qualification");
            v["deploy"]["test_count"] = json!(4);
        }),
        ("production with an evidence reference", |v| {
            v["deploy"]["admission_mode"] = json!("production");
            v["deploy"]["evidence_ref"] = json!("e".repeat(256));
        }),
        ("an evidence reference of 256 multi-byte characters", |v| {
            v["deploy"]["evidence_ref"] = json!("\u{e9}".repeat(256));
        }),
        ("a rollback naming a member", |v| {
            remove(v, "deploy");
            v["op"] = json!("rollback");
            v["rollback"] = json!({"reason": "bad release", "deployment_id": "speech-tts"});
        }),
        ("a rollback with no payload", |v| {
            remove(v, "deploy");
            v["op"] = json!("rollback");
        }),
        ("an undeploy with a reason", |v| {
            remove(v, "deploy");
            v["op"] = json!("undeploy");
            v["undeploy"] = json!({"deployment_id": "speech-tts", "reason": "retired"});
        }),
        ("a recover", |v| {
            remove(v, "deploy");
            v["op"] = json!("recover");
            v["recover"] = json!({"deployment_id": "speech-tts"});
        }),
        ("the SDK's status query", |v| {
            remove(v, "deploy");
            remove(v, "correlation_id");
            v["op"] = json!("status");
        }),
    ];
    for (label, edit) in cases {
        let mut request = deploy_base();
        edit(&mut request);
        let errors = schema_error_paths(&request);
        assert!(
            errors.is_empty(),
            "{label}: the schema refused it at {errors:?}"
        );
        decode_request(&request).unwrap_or_else(|e| panic!("{label}: the decoder refused it: {e}"));
    }
}

#[test]
fn defaults_are_omitted_on_the_wire() {
    // An explicit `replace` or `production` decodes to the default and is
    // written back without the field, which is what keeps a default deploy
    // byte-for-byte the request earlier clients send.
    let mut request = deploy_base();
    request["deploy"]["set_operation"] = json!("replace");
    let decoded = decode_request(&request).expect("decodes");
    let deploy = decoded.deploy.as_ref().expect("deploy");
    assert_eq!(deploy.set_operation, SetOperation::Replace);
    assert_eq!(
        serde_json::to_value(&decoded).expect("encode"),
        deploy_base(),
        "`replace` is written as an absent field"
    );
    let built = ControlRequest::undeploy(
        None,
        MemberRequest {
            deployment_id: "speech-tts".into(),
            reason: None,
        },
    );
    assert_eq!(
        serde_json::to_string(&built).expect("encode"),
        r#"{"schema_version":"0.1","op":"undeploy","undeploy":{"deployment_id":"speech-tts"}}"#
    );
}

// ---- The resident-set status projection -------------------------------------

#[test]
fn a_two_member_set_projects_every_member_and_no_singleton() {
    let set = committed_set("agent_state_0_2_two_member_set.json");
    let status = ResidentSetStatus::from_committed(&set);
    assert_eq!(status.set_id, set.set_id);
    assert_eq!(status.revision, set.revision);
    let ids: Vec<(&str, u64)> = status
        .members
        .iter()
        .map(|m| (m.deployment_id.as_str(), m.generation))
        .collect();
    assert_eq!(ids, [("speech-stt", 3), ("speech-tts", 5)]);
    for (member, committed) in status.members.iter().zip(&set.members) {
        assert_eq!(member.bundle_digest, committed.bundle_digest);
        assert_eq!(member.quota, committed.quota);
        assert_eq!(member.state, committed.state);
        assert_eq!(member.admission_mode, committed.admission_mode);
        let entry = set
            .endpoint_map
            .iter()
            .find(|e| e.deployment_id == committed.deployment_id)
            .expect("endpoint");
        assert_eq!(member.stream_endpoint, entry.stream_endpoint);
        assert_eq!(member.unary_endpoint, entry.unary_endpoint);
        // No source exists for these yet, so nothing is invented.
        assert_eq!(member.stream_api_version, None);
        assert_eq!(member.effective_quota, None);
        assert_eq!(member.staged_bytes, None);
        assert_eq!(member.contact, None);
    }
    assert_eq!(singleton_slots(&set), (None, None));
    let response = json!({
        "schema_version": "0.1", "status": "ok",
        "agent_status": {"agent_state": "ready", "resident_set": status}
    });
    let errors = schema_error_paths(&response);
    assert!(errors.is_empty(), "schema errors at {errors:?}");
}

#[test]
fn a_size_one_set_projects_into_the_singleton_slots() {
    let set = committed_set("agent_state_0_2_restore_step.json");
    let (active, previous) = singleton_slots(&set);
    let active = active.expect("the only serving member is active");
    let member = &set.members[0];
    assert_eq!(active.deployment_id, member.deployment_id);
    assert_eq!(active.bundle_digest, member.bundle_digest);
    assert_eq!(
        active.staged_path.as_deref(),
        Some(member.staged_path.as_str())
    );
    assert_eq!(
        active.serving_url.as_deref(),
        Some("http://127.0.0.1:18080/infer"),
        "the unary endpoint's `/infer` URL, the form clients discover"
    );
    let retained = member.previous.as_ref().expect("fixture retains one");
    let previous = previous.expect("the retained generation is previous_active");
    assert_eq!(previous.deployment_id, retained.deployment_id);
    assert_eq!(previous.serving_url, None);
    let response = json!({
        "schema_version": "0.1", "status": "ok",
        "agent_status": {
            "agent_state": "ready", "active": active, "previous_active": previous,
            "resident_set": ResidentSetStatus::from_committed(&set)
        }
    });
    let errors = schema_error_paths(&response);
    assert!(errors.is_empty(), "schema errors at {errors:?}");
}

#[test]
fn a_size_one_set_projects_a_serving_url_only_from_a_loopback_origin() {
    let base = committed_set("agent_state_0_2_restore_step.json");
    let cases: [(&str, Option<&str>); 5] = [
        (
            "http://localhost:18080",
            Some("http://localhost:18080/infer"),
        ),
        ("http://127.0.0.1:18080/infer", None),
        ("https://127.0.0.1:18080", None),
        ("http://example.invalid:18080", None),
        ("http://127.0.0.1:", None),
    ];
    for (endpoint, expected) in cases {
        let mut set = base.clone();
        set.endpoint_map[0].unary_endpoint = Some(endpoint.into());
        let (active, _) = singleton_slots(&set);
        assert_eq!(
            active.expect("active").serving_url.as_deref(),
            expected,
            "{endpoint}"
        );
    }
    let mut stream_only = base.clone();
    stream_only.endpoint_map[0].unary_endpoint = None;
    stream_only.endpoint_map[0].stream_endpoint = Some("127.0.0.1:18105".into());
    let (active, _) = singleton_slots(&stream_only);
    assert_eq!(active.expect("active").serving_url, None);
}

#[test]
fn an_endpoint_of_another_generation_is_not_attached() {
    let mut set = committed_set("agent_state_0_2_restore_step.json");
    set.endpoint_map[0].generation += 1;
    let status = ResidentSetStatus::from_committed(&set);
    assert_eq!(status.members[0].unary_endpoint, None);
    let (active, _) = singleton_slots(&set);
    assert_eq!(active.expect("active").serving_url, None);
}

#[test]
fn the_schema_refuses_an_unknown_member_status_field() {
    // Status is read leniently by clients, so this rule is the schema's.
    let set = committed_set("agent_state_0_2_two_member_set.json");
    let mut response = json!({
        "schema_version": "0.1", "status": "ok",
        "agent_status": {"agent_state": "ready", "resident_set": ResidentSetStatus::from_committed(&set)}
    });
    response["agent_status"]["resident_set"]["members"][0]["later"] = json!(1);
    assert_eq!(
        schema_error_paths(&response),
        BTreeSet::from(["/agent_status/resident_set/members/0".to_string()])
    );
}

#[test]
fn a_quarantined_only_member_projects_no_singleton() {
    let mut set = committed_set("agent_state_0_2_restore_step.json");
    set.members[0].state = MemberState::Quarantined;
    set.endpoint_map.clear();
    assert_eq!(singleton_slots(&set), (None, None));
    let status = ResidentSetStatus::from_committed(&set);
    assert_eq!(status.members[0].state, MemberState::Quarantined);
    assert_eq!(status.members[0].unary_endpoint, None);
}
