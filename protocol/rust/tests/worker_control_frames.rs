// SPDX-License-Identifier: Apache-2.0
//
// The runtime control channel's golden frames and schema.
//
// `tests/fixtures/worker_control_*.jsonl` hold one frame per line, exactly
// as the channel carries them: a request, then the response that answers
// it. They are authored to `protocol/schemas/worker_control.json`; every
// id, quota, timestamp and message in them is synthetic. This suite holds
// the Rust mirror to them byte for byte (decode, validate, re-encode, and
// compare the bytes), validates them against the schema with a real
// Draft-07 validator, keeps the schema's enums in step with the Rust
// enums, and checks single-fault frames against both readers for their own
// reason. Any other implementation of the channel reproduces the same
// bytes from the same values.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::{json, Value};
use tensorplate_protocol::{
    decode_with_version_check, encode_frame, DecodeError, ErrorCode, LedgerStatus, MemberQuota,
    MemberRef, PressureDirective, PressureLevel, WorkerControlFrameError, WorkerControlRequest,
    WorkerControlResponse, WorkerControlResponseError, WorkerError, WorkerOp, WorkerStatusOutcome,
    MAX_MEMBER_SESSIONS, WORKER_CONTROL_MAX_FRAME_BYTES, WORKER_CONTROL_MAX_LEDGER_SESSIONS,
};

const FRAME_FILES: [&str; 8] = [
    "worker_control_activate.jsonl",
    "worker_control_admission_fence.jsonl",
    "worker_control_error.jsonl",
    "worker_control_ledger_status.jsonl",
    "worker_control_member_mismatch.jsonl",
    "worker_control_pressure_directive.jsonl",
    "worker_control_quota_assign.jsonl",
    "worker_control_retire.jsonl",
];

const FENCE_TX: &str = "tx-00000000-0000-4000-8000-000000000001";

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load_bytes(name: &str) -> Vec<u8> {
    let p = fixtures_dir().join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
}

/// Every line of every golden file, with a label.
fn frames() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for name in FRAME_FILES {
        let bytes = load_bytes(name);
        assert!(
            bytes.ends_with(b"\n"),
            "{name}: last frame lacks its newline"
        );
        assert!(
            !bytes.contains(&b'\r'),
            "{name}: a frame ends with a single newline, never a carriage return"
        );
        let text = String::from_utf8(bytes).expect("utf8");
        for (index, line) in text.lines().enumerate() {
            out.push((format!("{name}:{}", index + 1), line.to_string()));
        }
    }
    out
}

fn golden_lines(responses: bool) -> Vec<String> {
    frames()
        .into_iter()
        .filter(|(_, line)| is_response(&serde_json::from_str(line).expect("json")) == responses)
        .map(|(_, line)| line)
        .collect()
}

fn read_schema(file: &str) -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../schemas")
        .join(file);
    let raw =
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read schema {}: {e}", p.display()));
    serde_json::from_str(&raw).expect("schema parses")
}

fn schema_document() -> Value {
    read_schema("worker_control.json")
}

fn validator() -> &'static jsonschema::JSONSchema {
    static VALIDATOR: std::sync::OnceLock<jsonschema::JSONSchema> = std::sync::OnceLock::new();
    VALIDATOR.get_or_init(|| {
        jsonschema::JSONSchema::compile(&schema_document()).expect("schema compiles as Draft-07")
    })
}

/// A validator whose root is one envelope definition, so its errors name
/// the failing location; through the whole schema's root `oneOf` every
/// failure is reported at the root.
fn envelope_validator(definition: &str) -> jsonschema::JSONSchema {
    let schema = schema_document();
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
    frame.get("status").is_some()
}

/// Where the frame's own envelope definition reports errors; empty when
/// the whole schema accepts the frame. A frame the envelope refuses must
/// also be refused by the whole schema.
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

/// Decode a frame the way a reader on the channel does, and re-encode it.
fn decode_and_reencode(line: &str) -> Result<Vec<u8>, DecodeError> {
    let value: Value = serde_json::from_str(line)?;
    if is_response(&value) {
        let response: WorkerControlResponse = decode_with_version_check(line)?;
        Ok(encode_frame(&response).expect("encode"))
    } else {
        let request: WorkerControlRequest = decode_with_version_check(line)?;
        Ok(encode_frame(&request).expect("encode"))
    }
}

fn l4_quota() -> MemberQuota {
    serde_json::from_value(json!({
        "session_count": 1, "domain_bytes": {"guest_ram": 134_217_728, "device_vram": 536_870_912}
    }))
    .expect("quota")
}

fn shared_quota() -> MemberQuota {
    serde_json::from_value(
        json!({"session_count": 1, "domain_bytes": {"shared_pool": 268_435_456}}),
    )
    .expect("quota")
}

// ---- Golden frames ------------------------------------------------------

#[test]
fn the_frame_files_are_exactly_the_documented_set() {
    let mut found: Vec<String> = std::fs::read_dir(fixtures_dir())
        .expect("read fixtures dir")
        .map(|e| e.expect("entry").file_name().into_string().expect("utf8"))
        .filter(|n| n.starts_with("worker_control_"))
        .collect();
    found.sort();
    assert_eq!(found, FRAME_FILES);
}

#[test]
fn every_golden_frame_is_schema_valid_and_reencodes_byte_for_byte() {
    for (label, line) in frames() {
        let value: Value = serde_json::from_str(&line).expect("json");
        let errors = schema_error_paths(&value);
        assert!(errors.is_empty(), "{label}: schema errors at {errors:?}");
        let reencoded =
            decode_and_reencode(&line).unwrap_or_else(|e| panic!("{label}: decode: {e}"));
        assert_eq!(
            String::from_utf8(reencoded).expect("utf8"),
            format!("{line}\n"),
            "{label}: the mirror does not reproduce the frame's bytes"
        );
    }
}

#[test]
fn every_response_answers_the_request_before_it() {
    for name in FRAME_FILES {
        let text = String::from_utf8(load_bytes(name)).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines.len() % 2 == 0,
            "{name}: frames come in request/response pairs"
        );
        for pair in lines.chunks(2) {
            let request: WorkerControlRequest =
                decode_with_version_check(pair[0]).expect("request");
            let response: WorkerControlResponse =
                decode_with_version_check(pair[1]).expect("response");
            response
                .answers(&request)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }
}

/// Every operation. Adding a `WorkerOp` variant fails to compile in the
/// match until it is handled; the list beside it must gain the variant too,
/// and the schema comparison fails until the schema does.
fn all_ops() -> Vec<WorkerOp> {
    let all = vec![
        WorkerOp::Prepare,
        WorkerOp::CapacityCheck,
        WorkerOp::Warm,
        WorkerOp::Promote,
        WorkerOp::ActiveStatus,
        WorkerOp::Unload,
        WorkerOp::AdmissionFence,
        WorkerOp::Activate,
        WorkerOp::Retire,
        WorkerOp::QuotaAssign,
        WorkerOp::LedgerStatus,
        WorkerOp::PressureDirective,
    ];
    for op in &all {
        match op {
            WorkerOp::Prepare
            | WorkerOp::CapacityCheck
            | WorkerOp::Warm
            | WorkerOp::Promote
            | WorkerOp::ActiveStatus
            | WorkerOp::Unload
            | WorkerOp::AdmissionFence
            | WorkerOp::Activate
            | WorkerOp::Retire
            | WorkerOp::QuotaAssign
            | WorkerOp::LedgerStatus
            | WorkerOp::PressureDirective => {}
        }
    }
    all
}

fn runtime_ops() -> Vec<WorkerOp> {
    all_ops().into_iter().filter(|op| op.is_runtime()).collect()
}

fn spelling<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .expect("spelling")
        .as_str()
        .expect("str")
        .to_string()
}

#[test]
fn every_runtime_op_has_a_golden_request_and_ok_response() {
    let mut requested = BTreeSet::new();
    let mut answered_ok = BTreeSet::new();
    for (_, line) in frames() {
        let value: Value = serde_json::from_str(&line).expect("json");
        let op = value["op"].as_str().expect("op").to_string();
        if is_response(&value) {
            if value["status"] == "ok" {
                answered_ok.insert(op);
            }
        } else {
            requested.insert(op);
        }
    }
    let expected: BTreeSet<String> = runtime_ops().into_iter().map(spelling).collect();
    assert_eq!(requested, expected);
    assert_eq!(answered_ok, expected);
}

#[test]
fn every_pressure_level_has_a_golden_request() {
    let mut levels = BTreeSet::new();
    for line in golden_lines(false) {
        let value: Value = serde_json::from_str(&line).expect("json");
        if let Some(level) = value["pressure"]["level"].as_str() {
            levels.insert(level.to_string());
        }
    }
    let expected: BTreeSet<String> = all_levels().into_iter().map(spelling).collect();
    assert_eq!(levels, expected);
}

// ---- Drift between the schema and the Rust mirror -----------------------

fn spellings<T: serde::Serialize + Copy>(values: &[T]) -> Value {
    Value::Array(values.iter().map(|v| Value::String(spelling(*v))).collect())
}

fn all_statuses() -> Vec<WorkerStatusOutcome> {
    let all = vec![
        WorkerStatusOutcome::Ok,
        WorkerStatusOutcome::Error,
        WorkerStatusOutcome::NotReady,
        WorkerStatusOutcome::Timeout,
        WorkerStatusOutcome::Unsupported,
        WorkerStatusOutcome::MemberMismatch,
    ];
    for status in &all {
        match status {
            WorkerStatusOutcome::Ok
            | WorkerStatusOutcome::Error
            | WorkerStatusOutcome::NotReady
            | WorkerStatusOutcome::Timeout
            | WorkerStatusOutcome::Unsupported
            | WorkerStatusOutcome::MemberMismatch => {}
        }
    }
    all
}

fn all_levels() -> Vec<PressureLevel> {
    let all = vec![
        PressureLevel::Normal,
        PressureLevel::ShedAdmission,
        PressureLevel::TerminateNewest,
        PressureLevel::Resume,
    ];
    for level in &all {
        match level {
            PressureLevel::Normal
            | PressureLevel::ShedAdmission
            | PressureLevel::TerminateNewest
            | PressureLevel::Resume => {}
        }
    }
    all
}

#[test]
fn schema_enums_match_the_mirror() {
    let schema = schema_document();
    let defs = &schema["definitions"];
    assert_eq!(
        defs["Request"]["properties"]["op"]["enum"],
        spellings(&all_ops())
    );
    assert_eq!(
        defs["Response"]["properties"]["op"]["enum"],
        spellings(&runtime_ops())
    );
    assert_eq!(
        defs["Response"]["properties"]["status"]["enum"],
        spellings(&all_statuses())
    );
    assert_eq!(
        defs["PressureDirective"]["properties"]["level"]["enum"],
        spellings(&all_levels())
    );
    for op in all_ops() {
        assert_eq!(
            op.to_string(),
            spelling(op),
            "WorkerOp's Display is its wire spelling"
        );
    }
}

#[test]
fn schema_op_lists_match_the_mirror() {
    let schema = schema_document();
    let defs = &schema["definitions"];
    let runtime = spellings(&runtime_ops());
    let transactional: Vec<WorkerOp> = runtime_ops()
        .into_iter()
        .filter(|op| op.is_transactional())
        .collect();
    let untransacted: Vec<WorkerOp> = runtime_ops()
        .into_iter()
        .filter(|op| !op.is_transactional())
        .collect();
    for envelope in ["Request", "Response"] {
        let rules = defs[envelope]["allOf"].as_array().expect("allOf");
        // The transaction-id rules partition the runtime ops the way the
        // mirror does.
        let partition: Vec<Value> = rules
            .iter()
            .filter(|r| {
                let rule = &r["then"]["properties"]["transaction_id"];
                rule.get("const").is_some() || rule.get("not").is_some()
            })
            .map(|r| r["if"]["properties"]["op"]["enum"].clone())
            .collect();
        assert_eq!(
            partition,
            vec![spellings(&transactional), spellings(&untransacted)],
            "{envelope}"
        );
    }
    // Every Request rule that names all the runtime ops names exactly them.
    let mut runtime_rules = 0;
    for rule in defs["Request"]["allOf"].as_array().expect("allOf") {
        for list in [
            &rule["if"]["properties"]["op"]["enum"],
            &rule["then"]["properties"]["op"]["enum"],
        ] {
            if list
                .as_array()
                .is_some_and(|l| l.len() == runtime_ops().len())
            {
                assert_eq!(*list, runtime, "a Request rule's runtime-op list drifted");
                runtime_rules += 1;
            }
        }
    }
    assert_eq!(
        runtime_rules, 4,
        "the Request rules that name every runtime op"
    );
}

#[test]
fn the_quota_and_member_definitions_match_the_state_file() {
    let schema = schema_document();
    let state = read_schema("agent_state.json");
    for definition in ["MemberQuota", "DomainQuotaBytes", "QuotaBytes"] {
        assert_eq!(
            schema["definitions"][definition], state["definitions"][definition],
            "{definition} differs between worker_control.json and agent_state.json"
        );
    }
    let member = &schema["definitions"]["MemberRef"]["properties"];
    let id = &state["definitions"]["DeploymentId"];
    for keyword in ["type", "minLength", "maxLength", "pattern", "not"] {
        assert_eq!(
            member["deployment_id"][keyword], id[keyword],
            "deployment_id.{keyword}"
        );
    }
    assert_eq!(member["generation"], state["definitions"]["StateCounter"]);
}

#[test]
fn the_documented_bounds_are_the_constants() {
    let schema = schema_document();
    let description = schema["description"].as_str().expect("description");
    assert!(description.contains(&format!(
        "at most {WORKER_CONTROL_MAX_FRAME_BYTES} bytes including the newline"
    )));
    assert!(description.contains(&format!(
        "A ledger reports at most {WORKER_CONTROL_MAX_LEDGER_SESSIONS} sessions"
    )));
    let ledger = &schema["definitions"]["LedgerStatus"]["properties"];
    let bound = json!(WORKER_CONTROL_MAX_LEDGER_SESSIONS);
    for count in ["reserved", "active", "closing", "ceiling"] {
        assert_eq!(ledger[count]["maximum"], bound, "LedgerStatus.{count}");
    }
    assert_eq!(ledger["admission_monotonic_ns"]["maxItems"], bound);
    assert_eq!(
        schema["definitions"]["PressureDirective"]["properties"]["count"]["maximum"],
        bound
    );
    assert_eq!(
        schema["definitions"]["MemberQuota"]["properties"]["session_count"]["maximum"],
        json!(MAX_MEMBER_SESSIONS)
    );
    assert_eq!(WORKER_CONTROL_MAX_LEDGER_SESSIONS, MAX_MEMBER_SESSIONS);
}

// ---- Frame bounds -------------------------------------------------------

#[test]
fn the_frame_limit_is_exact() {
    // A JSON string of `len` characters encodes to len + 2 quotes + newline.
    let at_limit = "x".repeat(WORKER_CONTROL_MAX_FRAME_BYTES - 3);
    assert_eq!(
        encode_frame(&at_limit).expect("a frame at the limit").len(),
        WORKER_CONTROL_MAX_FRAME_BYTES
    );
    let over = "x".repeat(WORKER_CONTROL_MAX_FRAME_BYTES - 2);
    assert!(matches!(
        encode_frame(&over),
        Err(WorkerControlFrameError::TooLarge(len)) if len == WORKER_CONTROL_MAX_FRAME_BYTES + 1
    ));
}

#[test]
fn the_largest_legal_ledger_answer_fits_one_frame() {
    let longest_id = "d".repeat(128);
    let largest_generation = 9_007_199_254_740_991;
    let request = WorkerControlRequest::ledger_status(
        "c".repeat(64),
        MemberRef::new(longest_id.clone(), largest_generation),
    );
    let sessions = WORKER_CONTROL_MAX_LEDGER_SESSIONS;
    let ledger = LedgerStatus {
        reserved: 0,
        active: sessions,
        closing: 0,
        ceiling: sessions,
        admission_monotonic_ns: vec![u64::MAX; sessions as usize],
    };
    let answer = WorkerControlResponse::answer(
        &request,
        MemberRef::new(longest_id, largest_generation),
        WorkerStatusOutcome::Ok,
    )
    .with_ledger(ledger);
    let frame = encode_frame(&answer).expect("the largest legal ledger fits");
    let line = String::from_utf8(frame).expect("utf8");
    decode_with_version_check::<WorkerControlResponse>(line.trim_end()).expect("and decodes");
}

// ---- Constructors -------------------------------------------------------

#[test]
fn request_constructors_build_the_golden_frames() {
    let m4 = MemberRef::new("speech-tts", 4);
    let m5 = MemberRef::new("speech-tts", 5);
    let level = |id: &str, level| {
        WorkerControlRequest::pressure_directive(id, m5.clone(), PressureDirective::level(level))
    };
    let built = [
        WorkerControlRequest::admission_fence(FENCE_TX, "ctl-000001", m4.clone()),
        WorkerControlRequest::activate(FENCE_TX, "ctl-000002", m5.clone()),
        WorkerControlRequest::retire(FENCE_TX, "ctl-000003", m4.clone(), 30_000),
        WorkerControlRequest::quota_assign("ctl-000004", m5.clone(), l4_quota()),
        WorkerControlRequest::ledger_status("ctl-000005", m5.clone()),
        level("ctl-000006", PressureLevel::ShedAdmission),
        WorkerControlRequest::pressure_directive(
            "ctl-000007",
            m5.clone(),
            PressureDirective::terminate_newest(1),
        ),
        WorkerControlRequest::ledger_status("ctl-000008", m4),
        level("ctl-000009", PressureLevel::Normal),
        level("ctl-000010", PressureLevel::Resume),
        WorkerControlRequest::quota_assign(
            "ctl-000011",
            MemberRef::new("vision-edge", 7),
            shared_quota(),
        ),
        WorkerControlRequest::activate(FENCE_TX, "ctl-000012", m5.clone()),
    ];
    let built: BTreeSet<String> = built
        .into_iter()
        .map(|request| {
            String::from_utf8(encode_frame(&request.with_timeout_ms(1_000)).expect("encode"))
                .expect("utf8")
        })
        .collect();
    let golden: BTreeSet<String> = golden_lines(false)
        .into_iter()
        .map(|g| format!("{g}\n"))
        .collect();
    assert_eq!(
        built, golden,
        "the constructors rebuild exactly the golden requests"
    );
}

#[test]
fn response_constructors_build_the_golden_answers() {
    let m4 = MemberRef::new("speech-tts", 4);
    let m5 = MemberRef::new("speech-tts", 5);
    let edge = MemberRef::new("vision-edge", 7);
    let ok = WorkerStatusOutcome::Ok;
    let answer = |request: &WorkerControlRequest, by: &MemberRef, status| {
        WorkerControlResponse::answer(request, by.clone(), status)
    };
    let level = |id: &str, level| {
        WorkerControlRequest::pressure_directive(id, m5.clone(), PressureDirective::level(level))
    };
    let built = [
        answer(
            &WorkerControlRequest::admission_fence(FENCE_TX, "ctl-000001", m4.clone()),
            &m4,
            ok,
        ),
        answer(
            &WorkerControlRequest::activate(FENCE_TX, "ctl-000002", m5.clone()),
            &m5,
            ok,
        )
        .with_quota(l4_quota()),
        answer(
            &WorkerControlRequest::retire(FENCE_TX, "ctl-000003", m4.clone(), 30_000),
            &m4,
            ok,
        ),
        answer(
            &WorkerControlRequest::quota_assign("ctl-000004", m5.clone(), l4_quota()),
            &m5,
            ok,
        )
        .with_quota(l4_quota()),
        answer(
            &WorkerControlRequest::ledger_status("ctl-000005", m5.clone()),
            &m5,
            ok,
        )
        .with_ledger(LedgerStatus {
            reserved: 0,
            active: 1,
            closing: 0,
            ceiling: 1,
            admission_monotonic_ns: vec![86_400_123_456_789],
        }),
        answer(&level("ctl-000006", PressureLevel::ShedAdmission), &m5, ok),
        answer(
            &WorkerControlRequest::pressure_directive(
                "ctl-000007",
                m5.clone(),
                PressureDirective::terminate_newest(1),
            ),
            &m5,
            ok,
        ),
        answer(
            &WorkerControlRequest::ledger_status("ctl-000008", m4.clone()),
            &m5,
            WorkerStatusOutcome::MemberMismatch,
        ),
        answer(&level("ctl-000009", PressureLevel::Normal), &m5, ok),
        answer(&level("ctl-000010", PressureLevel::Resume), &m5, ok),
        answer(
            &WorkerControlRequest::quota_assign("ctl-000011", edge.clone(), shared_quota()),
            &edge,
            ok,
        )
        .with_quota(shared_quota()),
        answer(
            &WorkerControlRequest::activate(FENCE_TX, "ctl-000012", m5.clone()),
            &m5,
            WorkerStatusOutcome::Error,
        )
        .with_error(WorkerError::new(
            ErrorCode::NotReady,
            "still warming: 1/2 engines loaded\n\"decoder\" pending (café)",
        )),
    ];
    let built: BTreeSet<String> = built
        .into_iter()
        .map(|response| String::from_utf8(encode_frame(&response).expect("encode")).expect("utf8"))
        .collect();
    let golden: BTreeSet<String> = golden_lines(true)
        .into_iter()
        .map(|g| format!("{g}\n"))
        .collect();
    assert_eq!(
        built, golden,
        "the constructors rebuild exactly the golden answers"
    );
}

#[test]
fn answers_refuses_a_response_to_another_request() {
    let m5 = MemberRef::new("speech-tts", 5);
    let request = WorkerControlRequest::ledger_status("ctl-1", m5.clone());
    let good = WorkerControlResponse::answer(&request, m5.clone(), WorkerStatusOutcome::Ok);
    good.answers(&request).expect("the answer");
    let mut legacy = request.clone();
    legacy.op = WorkerOp::ActiveStatus;
    let mut other_tx = good.clone();
    other_tx.transaction_id = Some("tx-2".into());
    let cases: Vec<(&str, WorkerControlResponse, WorkerControlRequest, &str)> = vec![
        (
            "another operation",
            good.clone(),
            WorkerControlRequest::pressure_directive(
                "ctl-1",
                m5.clone(),
                PressureDirective::level(PressureLevel::Normal),
            ),
            "operation differs",
        ),
        (
            "an answer to a legacy request, ids and member matching",
            WorkerControlResponse::answer(&legacy, m5.clone(), WorkerStatusOutcome::Ok),
            legacy,
            "operation differs",
        ),
        (
            "another transaction id",
            other_tx,
            request.clone(),
            "transaction_id differs",
        ),
        (
            "another correlation id",
            good,
            WorkerControlRequest::ledger_status("ctl-2", m5.clone()),
            "correlation_id differs",
        ),
        (
            "ok from another member",
            WorkerControlResponse::answer(
                &request,
                MemberRef::new("speech-tts", 4),
                WorkerStatusOutcome::Ok,
            ),
            request.clone(),
            "another member answered",
        ),
        (
            "member_mismatch from the member asked",
            WorkerControlResponse::answer(&request, m5, WorkerStatusOutcome::MemberMismatch),
            request,
            "member_mismatch from the member asked",
        ),
    ];
    for (label, response, asked, reason) in cases {
        assert_eq!(
            response.answers(&asked),
            Err(WorkerControlResponseError::NotAnAnswer(reason)),
            "{label}"
        );
    }
}

// ---- Verdicts: schema and decoder together ------------------------------

type Edit = fn(&mut Value);

fn line_of(name: &str, index: usize) -> Value {
    let text = String::from_utf8(load_bytes(name)).expect("utf8");
    serde_json::from_str(text.lines().nth(index).expect("line")).expect("json")
}

fn remove(v: &mut Value, key: &str) {
    v.as_object_mut().expect("object").remove(key);
}

struct Refusal {
    label: &'static str,
    base: (&'static str, usize),
    edit: Edit,
    schema_at: &'static str,
    decoder: &'static str,
}

const fn refusal(
    label: &'static str,
    base: (&'static str, usize),
    edit: Edit,
    schema_at: &'static str,
    decoder: &'static str,
) -> Refusal {
    Refusal {
        label,
        base,
        edit,
        schema_at,
        decoder,
    }
}

const FENCE_REQ: (&str, usize) = ("worker_control_admission_fence.jsonl", 0);
const FENCE_OK: (&str, usize) = ("worker_control_admission_fence.jsonl", 1);
const ACTIVATE_REQ: (&str, usize) = ("worker_control_activate.jsonl", 0);
const ACTIVATE_OK: (&str, usize) = ("worker_control_activate.jsonl", 1);
const RETIRE_REQ: (&str, usize) = ("worker_control_retire.jsonl", 0);
const QUOTA_REQ: (&str, usize) = ("worker_control_quota_assign.jsonl", 0);
const QUOTA_OK: (&str, usize) = ("worker_control_quota_assign.jsonl", 1);
const LEDGER_REQ: (&str, usize) = ("worker_control_ledger_status.jsonl", 0);
const LEDGER_OK: (&str, usize) = ("worker_control_ledger_status.jsonl", 1);
const PRESSURE_REQ: (&str, usize) = ("worker_control_pressure_directive.jsonl", 0);
const TERMINATE_REQ: (&str, usize) = ("worker_control_pressure_directive.jsonl", 2);
const ERROR_ANSWER: (&str, usize) = ("worker_control_error.jsonl", 1);
const MISMATCH_ANSWER: (&str, usize) = ("worker_control_member_mismatch.jsonl", 1);

/// Requests: ids, member and envelope.
#[allow(clippy::too_many_lines)]
fn request_envelope_refusals() -> Vec<Refusal> {
    vec![
        refusal(
            "a runtime request without a correlation id",
            LEDGER_REQ,
            |v| remove(v, "correlation_id"),
            "",
            "requires a correlation_id",
        ),
        refusal(
            "a runtime request with a malformed correlation id",
            LEDGER_REQ,
            |v| v["correlation_id"] = json!("ctl 1"),
            "/correlation_id",
            "correlation_id is invalid",
        ),
        refusal(
            "a null correlation id",
            LEDGER_REQ,
            |v| v["correlation_id"] = Value::Null,
            "/correlation_id",
            "invalid type: null",
        ),
        refusal(
            "an empty transaction id",
            FENCE_REQ,
            |v| v["transaction_id"] = json!(""),
            "/transaction_id",
            "transaction_id must be non-empty",
        ),
        refusal(
            "a malformed transaction id",
            FENCE_REQ,
            |v| v["transaction_id"] = json!("tx 1"),
            "/transaction_id",
            "transaction_id is invalid",
        ),
        refusal(
            "a runtime request without a member",
            LEDGER_REQ,
            |v| remove(v, "member"),
            "",
            "requires a member",
        ),
        refusal(
            "a member id with a slash",
            LEDGER_REQ,
            |v| v["member"]["deployment_id"] = json!("speech/tts"),
            "/member/deployment_id",
            "member is invalid: deployment_id",
        ),
        refusal(
            "member generation zero",
            LEDGER_REQ,
            |v| v["member"]["generation"] = json!(0),
            "/member/generation",
            "generation 0 is outside",
        ),
        refusal(
            "member generation past 2^53 - 1",
            LEDGER_REQ,
            |v| v["member"]["generation"] = json!(9_007_199_254_740_992_u64),
            "/member/generation",
            "generation 9007199254740992 is outside",
        ),
        refusal(
            "an unknown member field",
            LEDGER_REQ,
            |v| v["member"]["later"] = json!(1),
            "/member",
            "unknown field `later`",
        ),
        refusal(
            "a runtime request carrying a candidate id",
            LEDGER_REQ,
            |v| v["candidate_deployment_id"] = json!("speech-tts"),
            "",
            "must not carry a candidate",
        ),
        refusal(
            "a fence outside any deploy transaction",
            FENCE_REQ,
            |v| v["transaction_id"] = json!("runtime"),
            "/transaction_id",
            "wrong transaction_id",
        ),
        refusal(
            "an activate outside any deploy transaction",
            ACTIVATE_REQ,
            |v| v["transaction_id"] = json!("runtime"),
            "/transaction_id",
            "wrong transaction_id",
        ),
        refusal(
            "a ledger poll inside a deploy transaction",
            LEDGER_REQ,
            |v| v["transaction_id"] = json!("tx-1"),
            "/transaction_id",
            "wrong transaction_id",
        ),
        refusal(
            "a zero timeout",
            LEDGER_REQ,
            |v| v["timeout_ms"] = json!(0),
            "/timeout_ms",
            "timeout_ms must be at least 1",
        ),
        refusal(
            "a null timeout",
            LEDGER_REQ,
            |v| v["timeout_ms"] = Value::Null,
            "/timeout_ms",
            "invalid type: null",
        ),
        refusal(
            "an unknown op",
            LEDGER_REQ,
            |v| {
                remove(v, "member");
                v["op"] = json!("reload");
            },
            "/op",
            "unknown variant `reload`",
        ),
        refusal(
            "a legacy op carrying a member",
            LEDGER_REQ,
            |v| {
                v["op"] = json!("active_status");
                v["transaction_id"] = json!("tx-1");
            },
            "/op",
            "carries a runtime-operation field",
        ),
        refusal(
            "an unknown request field",
            LEDGER_REQ,
            |v| v["later"] = json!(1),
            "",
            "unknown field `later`",
        ),
    ]
}

/// Requests: payloads.
#[allow(clippy::too_many_lines)]
fn request_payload_refusals() -> Vec<Refusal> {
    vec![
        refusal(
            "a quota on a ledger poll",
            LEDGER_REQ,
            |v| v["quota"] = json!({"session_count": 0, "domain_bytes": {}}),
            "/op",
            "`quota` does not belong on op `ledger_status`",
        ),
        refusal(
            "a quota_assign without its quota",
            QUOTA_REQ,
            |v| remove(v, "quota"),
            "",
            "runtime op `quota_assign` requires `quota`",
        ),
        refusal(
            "a quota mixing shared_pool with guest_ram",
            QUOTA_REQ,
            |v| v["quota"]["domain_bytes"]["shared_pool"] = json!(1),
            "/quota/domain_bytes",
            "must not combine shared_pool",
        ),
        refusal(
            "a quota past the ledger bound",
            QUOTA_REQ,
            |v| v["quota"]["session_count"] = json!(2_049),
            "/quota/session_count",
            "session_count 2049 exceeds the 2048 sessions",
        ),
        refusal(
            "a pressure directive without its pressure",
            PRESSURE_REQ,
            |v| remove(v, "pressure"),
            "",
            "runtime op `pressure_directive` requires `pressure`",
        ),
        refusal(
            "a pressure without a level",
            PRESSURE_REQ,
            |v| v["pressure"] = json!({}),
            "/pressure",
            "missing field `level`",
        ),
        refusal(
            "a pressure on a ledger poll",
            LEDGER_REQ,
            |v| v["pressure"] = json!({"level": "normal"}),
            "/op",
            "`pressure` does not belong on op `ledger_status`",
        ),
        refusal(
            "an unknown pressure field",
            PRESSURE_REQ,
            |v| v["pressure"]["later"] = json!(1),
            "/pressure",
            "unknown field `later`",
        ),
        refusal(
            "terminate_newest without a count",
            TERMINATE_REQ,
            |v| remove(&mut v["pressure"], "count"),
            "/pressure",
            "terminate_newest needs a count",
        ),
        refusal(
            "terminate_newest with a zero count",
            TERMINATE_REQ,
            |v| v["pressure"]["count"] = json!(0),
            "/pressure/count",
            "terminate_newest needs a count",
        ),
        refusal(
            "terminate_newest past the ledger bound",
            TERMINATE_REQ,
            |v| v["pressure"]["count"] = json!(2_049),
            "/pressure/count",
            "terminate_newest needs a count",
        ),
        refusal(
            "a count on shed_admission",
            PRESSURE_REQ,
            |v| v["pressure"]["count"] = json!(1),
            "/pressure",
            "only terminate_newest carries a count",
        ),
        refusal(
            "an unknown pressure level",
            PRESSURE_REQ,
            |v| v["pressure"]["level"] = json!("panic"),
            "/pressure/level",
            "unknown variant `panic`",
        ),
        refusal(
            "a retire without a drain timeout",
            RETIRE_REQ,
            |v| remove(v, "drain_timeout_ms"),
            "",
            "runtime op `retire` requires `drain_timeout_ms`",
        ),
        refusal(
            "a zero drain timeout",
            RETIRE_REQ,
            |v| v["drain_timeout_ms"] = json!(0),
            "/drain_timeout_ms",
            "drain_timeout_ms must be at least 1",
        ),
        refusal(
            "a drain timeout on an activate",
            ACTIVATE_REQ,
            |v| v["drain_timeout_ms"] = json!(1),
            "/op",
            "`drain_timeout_ms` does not belong on op `activate`",
        ),
    ]
}

/// Responses: ids, member and envelope.
#[allow(clippy::too_many_lines)]
fn response_envelope_refusals() -> Vec<Refusal> {
    vec![
        refusal(
            "a runtime response without a transaction id",
            FENCE_OK,
            |v| remove(v, "transaction_id"),
            "",
            "must echo `transaction_id`",
        ),
        refusal(
            "a runtime response with a malformed transaction id",
            FENCE_OK,
            |v| v["transaction_id"] = json!("tx 1"),
            "/transaction_id",
            "echo is invalid: transaction_id",
        ),
        refusal(
            "a runtime response with the wrong transaction id",
            FENCE_OK,
            |v| v["transaction_id"] = json!("runtime"),
            "/transaction_id",
            "wrong transaction_id",
        ),
        refusal(
            "a ledger answer with a deploy transaction id",
            LEDGER_OK,
            |v| v["transaction_id"] = json!("tx-1"),
            "/transaction_id",
            "wrong transaction_id",
        ),
        refusal(
            "a runtime response without a correlation id",
            FENCE_OK,
            |v| remove(v, "correlation_id"),
            "",
            "must echo `correlation_id`",
        ),
        refusal(
            "a runtime response with a malformed correlation id",
            FENCE_OK,
            |v| v["correlation_id"] = json!("ctl 1"),
            "/correlation_id",
            "echo is invalid: correlation_id",
        ),
        refusal(
            "a runtime response without a member",
            LEDGER_OK,
            |v| remove(v, "member"),
            "",
            "must name the member that answered",
        ),
        refusal(
            "an answering member at generation zero",
            FENCE_OK,
            |v| v["member"]["generation"] = json!(0),
            "/member/generation",
            "the answering member is invalid",
        ),
        refusal(
            "a member on a legacy response",
            FENCE_OK,
            |v| remove(v, "op"),
            "",
            "belong only on a runtime response",
        ),
        refusal(
            "a legacy op on a response",
            FENCE_OK,
            |v| v["op"] = json!("warm"),
            "/op",
            "may name only a runtime op",
        ),
        refusal(
            "member_mismatch on a legacy response",
            FENCE_OK,
            |v| {
                remove(v, "op");
                remove(v, "member");
                v["status"] = json!("member_mismatch");
            },
            "",
            "belong only on a runtime response",
        ),
        refusal(
            "an unknown status",
            FENCE_OK,
            |v| v["status"] = json!("maybe"),
            "/status",
            "unknown variant `maybe`",
        ),
        refusal(
            "an unknown response field",
            FENCE_OK,
            |v| v["later"] = json!(1),
            "",
            "unknown field `later`",
        ),
        refusal(
            "an error as an array",
            ERROR_ANSWER,
            |v| v["error"] = json!(["not_ready", "x"]),
            "/error",
            "invalid type: sequence, expected a JSON object",
        ),
        refusal(
            "a null ready flag on a legacy answer",
            FENCE_OK,
            |v| {
                remove(v, "op");
                remove(v, "member");
                v["ready"] = Value::Null;
            },
            "/ready",
            "invalid type: null",
        ),
        refusal(
            "a ready flag on a runtime answer",
            FENCE_OK,
            |v| v["ready"] = json!(true),
            "",
            "belong only on a legacy response",
        ),
        refusal(
            "an active deployment id on a runtime answer",
            FENCE_OK,
            |v| v["active_deployment_id"] = json!("speech-tts"),
            "",
            "belong only on a legacy response",
        ),
        refusal(
            "a candidate deployment id on a runtime answer",
            FENCE_OK,
            |v| v["candidate_deployment_id"] = json!("speech-tts"),
            "",
            "belong only on a legacy response",
        ),
        refusal(
            "an error on an ok answer",
            FENCE_OK,
            |v| v["error"] = json!({"code": "internal", "message": "x"}),
            "",
            "must not carry an error",
        ),
        refusal(
            "an error on a member_mismatch answer",
            MISMATCH_ANSWER,
            |v| v["error"] = json!({"code": "internal", "message": "x"}),
            "",
            "must not carry an error",
        ),
        refusal(
            "a failed answer without its error",
            ERROR_ANSWER,
            |v| remove(v, "error"),
            "",
            "must carry its error",
        ),
        refusal(
            "a timeout answer without its error",
            FENCE_OK,
            |v| v["status"] = json!("timeout"),
            "",
            "must carry its error",
        ),
    ]
}

/// Responses: payloads.
#[allow(clippy::too_many_lines)]
fn response_payload_refusals() -> Vec<Refusal> {
    vec![
        refusal(
            "an ok ledger poll without its ledger",
            LEDGER_OK,
            |v| remove(v, "ledger"),
            "",
            "must carry the ledger",
        ),
        refusal(
            "a ledger on an error answer",
            LEDGER_OK,
            |v| {
                v["status"] = json!("error");
                v["error"] = json!({"code": "internal", "message": "x"});
            },
            "/status",
            "`ledger` does not belong on this response",
        ),
        refusal(
            "a ledger on another op",
            FENCE_OK,
            |v| v["ledger"] = json!({"reserved": 0, "active": 0, "closing": 0, "ceiling": 0, "admission_monotonic_ns": []}),
            "/op",
            "`ledger` does not belong on this response",
        ),
        refusal(
            "an unknown ledger field",
            LEDGER_OK,
            |v| v["ledger"]["later"] = json!(1),
            "/ledger",
            "unknown field `later`",
        ),
        refusal(
            "a ledger without its timestamps",
            LEDGER_OK,
            |v| remove(&mut v["ledger"], "admission_monotonic_ns"),
            "/ledger",
            "missing field `admission_monotonic_ns`",
        ),
        refusal(
            "a ledger ceiling past the bound",
            LEDGER_OK,
            |v| v["ledger"]["ceiling"] = json!(2_049),
            "/ledger/ceiling",
            "ceiling 2049 exceeds",
        ),
        refusal(
            "an ok activate without the quota in force",
            ACTIVATE_OK,
            |v| remove(v, "quota"),
            "",
            "must carry the quota in force",
        ),
        refusal(
            "an ok quota_assign without the quota in force",
            QUOTA_OK,
            |v| remove(v, "quota"),
            "",
            "must carry the quota in force",
        ),
        refusal(
            "a quota on a fence answer",
            FENCE_OK,
            |v| v["quota"] = json!({"session_count": 0, "domain_bytes": {}}),
            "/op",
            "`quota` does not belong on this response",
        ),
        refusal(
            "a quota on a failed activate",
            ERROR_ANSWER,
            |v| v["quota"] = json!({"session_count": 0, "domain_bytes": {}}),
            "/status",
            "`quota` does not belong on this response",
        ),
        refusal(
            "an answered quota mixing shared_pool with guest_ram",
            QUOTA_OK,
            |v| v["quota"]["domain_bytes"]["shared_pool"] = json!(1),
            "/quota/domain_bytes",
            "must not combine shared_pool",
        ),
        refusal(
            "an answered quota past the session bound",
            ACTIVATE_OK,
            |v| v["quota"]["session_count"] = json!(2_049),
            "/quota/session_count",
            "session_count 2049 exceeds the 2048 sessions",
        ),
    ]
}

#[test]
fn frames_both_readers_refuse() {
    let cases = request_envelope_refusals()
        .into_iter()
        .chain(request_payload_refusals())
        .chain(response_envelope_refusals())
        .chain(response_payload_refusals());
    for Refusal {
        label,
        base,
        edit,
        schema_at,
        decoder,
    } in cases
    {
        let mut doc = line_of(base.0, base.1);
        edit(&mut doc);
        // Exactly one location: a case that breaks two rules could pass for
        // the one it does not name.
        assert_eq!(
            schema_error_paths(&doc),
            BTreeSet::from([schema_at.to_string()]),
            "{label}: the schema's errors are not all at `{schema_at}`"
        );
        let line = serde_json::to_string(&doc).expect("encode");
        match decode_and_reencode(&line) {
            Ok(_) => panic!("{label}: the decoder accepted it"),
            Err(err) => assert!(
                err.to_string().contains(decoder),
                "{label}: expected the decoder to say `{decoder}`, got `{err}`"
            ),
        }
    }
}

/// Ledger rules Draft-07 cannot state: each case is schema-valid and
/// refused by the decoder with its own message.
#[test]
fn the_decoder_enforces_the_ledger_rules() {
    let cases: Vec<(&str, Edit, &str)> = vec![
        (
            "more sessions held than the ceiling",
            |v| v["ledger"]["closing"] = json!(1),
            "reserved + active + closing (2) exceeds ceiling (1)",
        ),
        (
            "a timestamp missing for an admitted session",
            |v| {
                v["ledger"]["reserved"] = json!(1);
                v["ledger"]["ceiling"] = json!(2);
            },
            "1 admission timestamps for 2 reserved or active sessions",
        ),
        (
            "a timestamp for a closing session",
            |v| {
                v["ledger"]["closing"] = json!(1);
                v["ledger"]["ceiling"] = json!(2);
                v["ledger"]["admission_monotonic_ns"] = json!([1, 2]);
            },
            "2 admission timestamps for 1 reserved or active sessions",
        ),
        (
            "timestamps newest first",
            |v| {
                v["ledger"]["active"] = json!(2);
                v["ledger"]["ceiling"] = json!(2);
                v["ledger"]["admission_monotonic_ns"] = json!([2, 1]);
            },
            "admission timestamps must be oldest first",
        ),
    ];
    for (label, edit, message) in cases {
        let mut doc = line_of(LEDGER_OK.0, LEDGER_OK.1);
        edit(&mut doc);
        let paths = schema_error_paths(&doc);
        assert!(
            paths.is_empty(),
            "{label}: the schema refused it at {paths:?}"
        );
        let line = serde_json::to_string(&doc).expect("encode");
        match decode_and_reencode(&line) {
            Err(DecodeError::InvalidPayload(got)) => {
                assert!(
                    got.ends_with(message),
                    "{label}: expected `{message}`, got `{got}`"
                );
            }
            other => panic!("{label}: expected InvalidPayload, got {other:?}"),
        }
    }
}

#[test]
fn frames_both_readers_accept() {
    let cases: Vec<(&str, (&str, usize), Edit)> = vec![
        ("equal admission timestamps", LEDGER_OK, |v| {
            v["ledger"]["active"] = json!(2);
            v["ledger"]["ceiling"] = json!(2);
            v["ledger"]["admission_monotonic_ns"] = json!([5, 5]);
        }),
        (
            "a ledger at the bound with nothing admitted",
            LEDGER_OK,
            |v| {
                v["ledger"]["active"] = json!(0);
                v["ledger"]["ceiling"] = json!(2_048);
                v["ledger"]["admission_monotonic_ns"] = json!([]);
            },
        ),
        ("terminate_newest at the ledger bound", TERMINATE_REQ, |v| {
            v["pressure"]["count"] = json!(2_048);
        }),
        ("a request without a timeout", LEDGER_REQ, |v| {
            remove(v, "timeout_ms");
        }),
        ("a member id at the length limit", LEDGER_REQ, |v| {
            v["member"]["deployment_id"] = json!("d".repeat(128));
        }),
    ];
    for (label, base, edit) in cases {
        let mut doc = line_of(base.0, base.1);
        edit(&mut doc);
        let paths = schema_error_paths(&doc);
        assert!(
            paths.is_empty(),
            "{label}: the schema refused it at {paths:?}"
        );
        let line = serde_json::to_string(&doc).expect("encode");
        decode_and_reencode(&line)
            .unwrap_or_else(|e| panic!("{label}: the decoder refused it: {e}"));
    }
}

/// Where the two readers knowingly disagree.
#[test]
fn documented_divergences() {
    // Draft-07 `type: integer` accepts an integral float; the decoder's
    // counters and ids are exact integers.
    let mut doc = line_of(LEDGER_REQ.0, LEDGER_REQ.1);
    doc["member"]["generation"] = json!(5.0);
    assert!(schema_error_paths(&doc).is_empty());
    assert!(matches!(
        decode_and_reencode(&serde_json::to_string(&doc).expect("encode")),
        Err(DecodeError::Malformed(_))
    ));
}

#[test]
fn legacy_frames_keep_decoding() {
    let prepare = json!({
        "schema_version": "0.1", "transaction_id": "tx-1", "op": "prepare",
        "candidate": {"deployment_id": "d", "staged_path": "/s", "bundle_digest": "sha256:00",
                      "backend_hint": "mock", "model_class": "vision"},
        "candidate_deployment_id": "d", "timeout_ms": 1000
    });
    assert!(schema_error_paths(&prepare).is_empty());
    let request: WorkerControlRequest =
        decode_with_version_check(&prepare.to_string()).expect("legacy request");
    assert_eq!(request.op, WorkerOp::Prepare);
    let ok =
        json!({"schema_version": "0.1", "transaction_id": "tx-1", "status": "ok", "ready": true});
    assert!(schema_error_paths(&ok).is_empty());
    decode_with_version_check::<WorkerControlResponse>(&ok.to_string()).expect("legacy response");
}
