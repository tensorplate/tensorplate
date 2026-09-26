// SPDX-License-Identifier: Apache-2.0
//
// The agent's durable state file, state versions 0.1 and 0.2: the committed
// fixtures against `protocol/schemas/agent_state.json` (a real Draft-07
// validator) and against `decode_agent_state`, the decoder the agent's state
// store uses, plus drift checks that keep the schema and the Rust mirror in
// lockstep.
//
// Fixtures:
//
// - `agent_state_0_1_legacy.json` is recorded: the agent's state store wrote
//   it at the 0.2.1 release commit, whose store and state record types are
//   unchanged since 0.1.1, after two deploys and a third refused for a
//   digest mismatch, driven through the coordinator with the mock worker
//   control. The two staging paths were rewritten from the test's temporary
//   directory to the packaged staging directory before the file was first
//   committed; `backend_hint: "mock"` is the test backend.
// - `agent_state_0_2_restore_step.json` is what this tree's state store
//   writes when that legacy state becomes a one-member set (the store's own
//   tests assert it byte for byte). After a completed write the backup holds
//   the same bytes, so this one file is both halves of the pair an older
//   agent meets on the downgrade restore step, and the backup half of an
//   interrupted first write. Its descriptor and configuration digests are
//   synthetic.
// - `agent_state_0_2_two_member_set.json` is authored to the schema: a
//   speech-to-text and a text-to-speech member on a discrete-GPU host, the
//   second with a retained previous generation. Every digest, quota and
//   endpoint in it is synthetic; no measured quota exists yet.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::{json, Value};
use tensorplate_protocol::agent_state::{
    AGENT_STATE_ROOT_KEYS, DEPLOYMENT_RECORD_KEYS, ERROR_RECORD_KEYS, QUARANTINE_RECORD_KEYS,
    TRANSACTION_RECORD_KEYS,
};
use tensorplate_protocol::{
    decode_agent_state, decode_with_version_check, AdmissionMode, AgentState, BudgetDomainName,
    DecodeError, DeployState, DomainQuotaBytes, ErrorCode, MemberState, ResidentMember,
    TransactionKind, AGENT_STATE_SCHEMA_VERSIONS, AGENT_STATE_SCHEMA_VERSION_LEGACY,
    AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET,
};

const LEGACY: &str = "agent_state_0_1_legacy.json";
const RESTORE: &str = "agent_state_0_2_restore_step.json";
const TWO_MEMBER: &str = "agent_state_0_2_two_member_set.json";
const FIXTURES: [&str; 3] = [LEGACY, RESTORE, TWO_MEMBER];

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load(name: &str) -> String {
    let p = fixtures_dir().join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
}

fn load_value(name: &str) -> Value {
    serde_json::from_str(&load(name)).expect("fixture parses")
}

fn schema_document() -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../schemas/agent_state.json");
    let raw =
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read schema {}: {e}", p.display()));
    serde_json::from_str(&raw).expect("schema document parses")
}

fn agent_control_schema() -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../schemas/agent_control.json");
    let raw =
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read schema {}: {e}", p.display()));
    serde_json::from_str(&raw).expect("schema document parses")
}

fn validator() -> &'static jsonschema::JSONSchema {
    static VALIDATOR: std::sync::OnceLock<jsonschema::JSONSchema> = std::sync::OnceLock::new();
    VALIDATOR.get_or_init(|| {
        jsonschema::JSONSchema::compile(&schema_document()).expect("schema compiles as Draft-07")
    })
}

fn schema_errors(instance: &Value) -> Vec<String> {
    match validator().validate(instance) {
        Ok(()) => Vec::new(),
        Err(errors) => errors
            .map(|e| format!("{e} at {}", e.instance_path))
            .collect(),
    }
}

fn decode(instance: &Value) -> Result<AgentState, DecodeError> {
    decode_agent_state(&serde_json::to_string(instance).expect("serialize"))
}

// ---- The committed fixtures -------------------------------------------------

#[test]
fn the_fixture_set_is_exactly_the_three_documented_files() {
    let mut found: Vec<String> = std::fs::read_dir(fixtures_dir())
        .expect("read fixtures dir")
        .map(|e| e.expect("entry").file_name().into_string().expect("utf8"))
        .filter(|n| n.starts_with("agent_state_"))
        .collect();
    found.sort();
    let mut expected: Vec<String> = FIXTURES.iter().map(|s| (*s).to_string()).collect();
    expected.sort();
    assert_eq!(found, expected);
}

#[test]
fn every_fixture_is_schema_valid_decodes_and_round_trips() {
    for name in FIXTURES {
        let instance = load_value(name);
        let errors = schema_errors(&instance);
        assert!(errors.is_empty(), "{name}: schema rejected it: {errors:?}");
        let decoded =
            decode_agent_state(&load(name)).unwrap_or_else(|e| panic!("{name}: decode: {e}"));
        let reencoded = serde_json::to_string(&decoded).expect("encode");
        assert_eq!(
            decode_agent_state(&reencoded).expect("re-decode"),
            decoded,
            "{name}: round trip changed the state"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&reencoded).expect("value"),
            instance,
            "{name}: re-encoding dropped or changed a field"
        );
        assert_eq!(
            decoded.schema_version,
            decoded.required_schema_version(),
            "{name}: stamped with a version other than the one its content needs"
        );
    }
}

#[test]
fn the_legacy_fixture_is_the_writers_exact_encoding() {
    // The store writes `serde_json::to_vec_pretty` with no trailing newline;
    // the committed file adds one.
    let raw = load(LEGACY);
    let decoded = decode_agent_state(&raw).expect("decode");
    assert_eq!(decoded.schema_version, AGENT_STATE_SCHEMA_VERSION_LEGACY);
    let encoded =
        String::from_utf8(serde_json::to_vec_pretty(&decoded).expect("encode")).expect("utf8");
    assert_eq!(format!("{encoded}\n"), raw);
    // What the recording covers.
    assert!(decoded.active.is_some() && decoded.previous_active.is_some());
    assert!(decoded.last_error.is_some() && decoded.quarantined.len() == 1);
    // Real epoch-nanosecond stamps exceed 2^53, so the state file is never
    // put through the byte-lexeme canonicalization other schemas use.
    assert!(
        decoded
            .active
            .as_ref()
            .and_then(|a| a.promoted_monotonic_ns)
            .expect("stamp")
            > (1_u64 << 53)
    );
}

#[test]
fn the_protocol_wide_decoder_refuses_every_0_2_fixture() {
    // Agents through 0.2.x read the state file through
    // `decode_with_version_check`, which stays pinned to "0.1".
    for name in [RESTORE, TWO_MEMBER] {
        match decode_with_version_check::<AgentState>(&load(name)).expect_err(name) {
            DecodeError::UnsupportedSchemaVersion { got, expected } => {
                assert_eq!(got, "0.2", "{name}");
                assert_eq!(expected, "0.1", "{name}");
            }
            other => panic!("{name}: expected UnsupportedSchemaVersion, got {other:?}"),
        }
    }
    decode_with_version_check::<AgentState>(&load(LEGACY)).expect("the legacy file stays 0.1");
}

// ---- Drift between the schema and the Rust mirror ---------------------------

#[test]
fn schema_versions_match_the_mirror() {
    let schema = schema_document();
    let root = &schema["properties"]["schema_version"];
    assert_eq!(root["enum"], json!(AGENT_STATE_SCHEMA_VERSIONS));
    assert!(root.get("const").is_none());
    assert_eq!(
        schema["allOf"][0]["if"]["properties"]["schema_version"]["const"],
        json!(AGENT_STATE_SCHEMA_VERSION_LEGACY)
    );
    assert_eq!(
        AGENT_STATE_SCHEMA_VERSIONS[1],
        AGENT_STATE_SCHEMA_VERSION_RESIDENT_SET
    );
}

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().expect("object").keys().cloned().collect()
}

#[test]
fn root_properties_match_the_decoders_key_list() {
    let schema = schema_document();
    let expected: BTreeSet<String> = AGENT_STATE_ROOT_KEYS
        .iter()
        .map(|k| (*k).to_string())
        .collect();
    assert_eq!(keys(&schema["properties"]), expected);
}

#[test]
fn shared_record_key_lists_match_the_schema() {
    let schema = schema_document();
    let defs = &schema["definitions"];
    for (definition, list) in [
        ("DeploymentRecord", &DEPLOYMENT_RECORD_KEYS[..]),
        ("TransactionRecord", &TRANSACTION_RECORD_KEYS[..]),
        ("ErrorRecord", &ERROR_RECORD_KEYS[..]),
        ("QuarantineRecord", &QUARANTINE_RECORD_KEYS[..]),
    ] {
        let expected: BTreeSet<String> = list.iter().map(|k| (*k).to_string()).collect();
        assert_eq!(
            keys(&defs[definition]["properties"]),
            expected,
            "{definition}: the decoder's key list differs from the schema"
        );
    }
}

#[test]
fn member_and_retained_generation_properties_match_the_mirror() {
    let schema = schema_document();
    let defs = &schema["definitions"];
    let fixture = decode_agent_state(&load(TWO_MEMBER)).expect("decode");
    let set = fixture.resident_set.expect("set");
    let member: &ResidentMember = &set.members[1];
    let member_keys = keys(&serde_json::to_value(member).expect("member"));
    let retained_keys =
        keys(&serde_json::to_value(member.previous.as_ref().expect("previous")).expect("retained"));
    // The fixture's second member sets every optional field but `labels`.
    let mut expected_member = member_keys.clone();
    expected_member.insert("labels".into());
    let mut expected_retained = retained_keys.clone();
    expected_retained.insert("labels".into());
    assert_eq!(keys(&defs["ResidentMember"]["properties"]), expected_member);
    assert_eq!(
        keys(&defs["RetainedGeneration"]["properties"]),
        expected_retained
    );

    let mut shared = expected_member.clone();
    shared.remove("state");
    shared.remove("previous");
    assert_eq!(
        shared, expected_retained,
        "a retained generation is a member minus state/previous"
    );

    let required = |def: &str| -> BTreeSet<String> {
        defs[def]["required"]
            .as_array()
            .expect("required")
            .iter()
            .map(|v| v.as_str().expect("str").to_string())
            .collect()
    };
    let optional = ["promoted_monotonic_ns", "labels", "previous"];
    let expected_required: BTreeSet<String> = expected_member
        .iter()
        .filter(|k| !optional.contains(&k.as_str()))
        .cloned()
        .collect();
    assert_eq!(required("ResidentMember"), expected_required);
    let mut expected_retained_required = expected_required.clone();
    expected_retained_required.remove("state");
    assert_eq!(required("RetainedGeneration"), expected_retained_required);

    for def in ["ResidentMember", "RetainedGeneration"] {
        assert_eq!(
            defs[def]["properties"]["admission_mode"]["enum"],
            json!([AdmissionMode::Production, AdmissionMode::Qualification])
        );
    }
    assert_eq!(
        defs["ResidentMember"]["properties"]["state"]["enum"],
        json!([MemberState::Serving, MemberState::Quarantined])
    );
}

/// Every [`BudgetDomainName`]. Adding a variant fails to compile here until
/// the match handles it; the list beside the match must gain it too (the
/// schema comparison below then fails until the schema does).
fn all_domains() -> [BudgetDomainName; 3] {
    fn exhaustive(domain: BudgetDomainName) {
        match domain {
            BudgetDomainName::SharedPool
            | BudgetDomainName::GuestRam
            | BudgetDomainName::DeviceVram => {}
        }
    }
    let all = [
        BudgetDomainName::SharedPool,
        BudgetDomainName::GuestRam,
        BudgetDomainName::DeviceVram,
    ];
    all.iter().copied().for_each(exhaustive);
    all
}

#[test]
fn quota_domains_are_the_budget_domain_vocabulary() {
    let spellings: BTreeSet<String> = all_domains()
        .iter()
        .map(|d| {
            serde_json::to_value(d)
                .expect("domain")
                .as_str()
                .expect("str")
                .to_string()
        })
        .collect();
    let schema = schema_document();
    assert_eq!(
        keys(&schema["definitions"]["DomainQuotaBytes"]["properties"]),
        spellings
    );
    let every = DomainQuotaBytes {
        shared_pool: Some(1),
        guest_ram: Some(2),
        device_vram: Some(3),
    };
    let rust_keys = keys(&serde_json::to_value(every).expect("quota bytes"));
    assert_eq!(rust_keys, spellings);
    for domain in all_domains() {
        let name = serde_json::to_value(domain).expect("domain");
        let key = name.as_str().expect("str");
        assert_eq!(
            every.get(domain),
            serde_json::to_value(every).expect("value")[key].as_u64(),
            "DomainQuotaBytes::get disagrees with the `{key}` field"
        );
    }
}

#[test]
fn the_id_rule_is_the_control_apis_deployment_id_rule() {
    let control = agent_control_schema();
    let control_id = &control["definitions"]["DeploymentId"];
    let state_id = &schema_document()["definitions"]["DeploymentId"];
    for keyword in ["type", "minLength", "maxLength", "pattern", "not"] {
        assert_eq!(
            state_id[keyword], control_id[keyword],
            "DeploymentId.{keyword} differs from the control API's DeploymentId"
        );
    }
    // Every deployment id the control API takes uses that definition.
    let defs = &control["definitions"];
    for (definition, property) in [
        ("DeployRequest", "deployment_id"),
        ("RollbackRequest", "deployment_id"),
        ("MemberRequest", "deployment_id"),
        ("MemberStatus", "deployment_id"),
    ] {
        assert_eq!(
            defs[definition]["properties"][property]["$ref"], "#/definitions/DeploymentId",
            "{definition}.{property}"
        );
    }
}

// ---- Verdicts: schema and decoder together ----------------------------------

type Edit = fn(&mut Value);

fn edited(base: &str, edit: Edit) -> Value {
    let mut v = load_value(base);
    edit(&mut v);
    v
}

/// The value at `pointer`, inserting its last key into an existing object
/// when absent.
fn at<'a>(v: &'a mut Value, pointer: &str) -> &'a mut Value {
    if v.pointer(pointer).is_none() {
        let (parent, key) = pointer.rsplit_once('/').expect("pointer");
        v.pointer_mut(parent)
            .and_then(Value::as_object_mut)
            .unwrap_or_else(|| panic!("no object at {parent} in the base document"))
            .insert(key.to_string(), Value::Null);
    }
    v.pointer_mut(pointer).expect("present")
}

fn remove(v: &mut Value, pointer: &str, key: &str) {
    at(v, pointer).as_object_mut().expect("object").remove(key);
}

const M0: &str = "/resident_set/members/0";
const M1: &str = "/resident_set/members/1";

/// Quarantine the first member and drop its endpoint, so a fault in that
/// member's own fields is the only fault in the document.
fn quarantine_first(v: &mut Value) {
    *at(v, &format!("{M0}/state")) = json!("quarantined");
    at(v, "/resident_set/endpoint_map")
        .as_array_mut()
        .expect("array")
        .remove(0);
}

/// One single-fault document both readers must refuse: the base fixture,
/// the edit, where the schema must report an error (a JSON pointer into the
/// document; "" is the root), and text the decoder's error must contain.
struct Refusal {
    label: &'static str,
    base: &'static str,
    edit: Edit,
    schema_at: &'static str,
    decoder: &'static str,
}

const fn refusal(
    label: &'static str,
    base: &'static str,
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

/// Every rule the schema states, and every rule the decoder enforces that
/// the schema can also state, with a case that breaks only that rule.
#[allow(clippy::too_many_lines)]
fn refused_by_both() -> Vec<Refusal> {
    const RS: &str = "/resident_set";
    const M0Q: &str = "/resident_set/members/0/quota";
    const PREV: &str = "/resident_set/members/1/previous";
    const E0: &str = "/resident_set/endpoint_map/0";
    vec![
        refusal(
            "an unknown state version",
            TWO_MEMBER,
            |v| v["schema_version"] = json!("0.3"),
            "/schema_version",
            "unsupported schema_version `0.3`",
        ),
        refusal(
            "a 0.1 stamp carrying the counter",
            LEGACY,
            |v| v["next_generation"] = json!(1),
            "",
            "cannot read (the generation counter",
        ),
        refusal(
            "a 0.1 stamp carrying a resident set, and so no counter",
            TWO_MEMBER,
            |v| {
                v["schema_version"] = json!("0.1");
                remove(v, "", "next_generation");
            },
            "",
            "cannot read (the generation counter",
        ),
        refusal(
            "a resident set without the counter",
            TWO_MEMBER,
            |v| remove(v, "", "next_generation"),
            "",
            "resident_set requires next_generation",
        ),
        refusal(
            "an active slot beside the set",
            TWO_MEMBER,
            |v| v["active"] = load_value(LEGACY)["active"].clone(),
            "",
            "singleton `active` slot",
        ),
        refusal(
            "a previous_active slot beside the set",
            TWO_MEMBER,
            |v| v["previous_active"] = load_value(LEGACY)["active"].clone(),
            "",
            "singleton `previous_active` slot",
        ),
        refusal(
            "a candidate slot beside the set",
            TWO_MEMBER,
            |v| v["candidate"] = load_value(LEGACY)["active"].clone(),
            "",
            "singleton `candidate` slot",
        ),
        refusal(
            "an unknown top-level field at 0.2",
            TWO_MEMBER,
            |v| v["later"] = json!(true),
            "",
            "unknown top-level field `later`",
        ),
        refusal(
            "an unknown last_error field at 0.2",
            RESTORE,
            |v| v["last_error"]["later"] = json!(1),
            "/last_error",
            "unknown field `later` in `last_error`",
        ),
        refusal(
            "an unknown quarantine field at 0.2",
            RESTORE,
            |v| v["quarantined"][0]["later"] = json!(1),
            "/quarantined/0",
            "unknown field `later` in `quarantined[0]`",
        ),
        refusal(
            "an unknown quarantine error field at 0.2",
            RESTORE,
            |v| v["quarantined"][0]["error"]["later"] = json!(1),
            "/quarantined/0/error",
            "unknown field `later` in `quarantined[0].error`",
        ),
        refusal(
            "an unknown in-flight transaction field at 0.2",
            RESTORE,
            |v| {
                v["in_flight_transaction"] = json!({
                    "transaction_id": "tx-1", "deployment_id": "vision-v2", "phase": "received",
                    "kind": "deploy", "later": 1
                });
            },
            "/in_flight_transaction",
            "unknown field `later` in `in_flight_transaction`",
        ),
        refusal(
            "an unknown transaction failure field at 0.2",
            RESTORE,
            |v| {
                v["in_flight_transaction"] = json!({
                    "transaction_id": "tx-1", "deployment_id": "vision-v2", "phase": "failed",
                    "kind": "deploy", "failure": {"code": "internal", "message": "x", "later": 1}
                });
            },
            "/in_flight_transaction/failure",
            "unknown field `later` in `in_flight_transaction.failure`",
        ),
        refusal(
            "an unknown singleton field in a 0.2 state without a set",
            RESTORE,
            |v| {
                remove(v, "", "resident_set");
                v["active"] = load_value(LEGACY)["active"].clone();
                v["active"]["later"] = json!(1);
            },
            "/active",
            "unknown field `later` in `active`",
        ),
        refusal(
            "an array-form transaction hiding a failure field at 0.2",
            RESTORE,
            |v| {
                v["in_flight_transaction"] = json!([
                    "tx-1", "vision-v2", "received", "deploy", null, null, null, null, null,
                    {"code": "internal", "message": "x", "later": 1}
                ]);
            },
            "/in_flight_transaction",
            "`in_flight_transaction` of a schema_version 0.2 state must be a JSON object",
        ),
        refusal(
            "an array-form transaction failure at 0.2",
            RESTORE,
            |v| {
                v["in_flight_transaction"] = json!({
                    "transaction_id": "tx-1", "deployment_id": "vision-v2", "phase": "failed",
                    "kind": "deploy", "failure": ["internal", "x", null, false]
                });
            },
            "/in_flight_transaction/failure",
            "`in_flight_transaction.failure` of a schema_version 0.2 state must be a JSON object",
        ),
        refusal(
            "an array-form quarantine entry at 0.2",
            RESTORE,
            |v| {
                let entry = v["quarantined"][0].clone();
                v["quarantined"][0] = json!([
                    entry["transaction_id"],
                    entry["deployment_id"],
                    entry["bundle_digest"],
                    entry["phase"],
                    entry["error"],
                    entry["quarantined_monotonic_ns"]
                ]);
            },
            "/quarantined/0",
            "`quarantined[0]` of a schema_version 0.2 state must be a JSON object",
        ),
        refusal(
            "an array-form quarantine error at 0.2",
            RESTORE,
            |v| v["quarantined"][0]["error"] = json!(["internal", "x", null]),
            "/quarantined/0/error",
            "`quarantined[0].error` of a schema_version 0.2 state must be a JSON object",
        ),
        refusal(
            "an array-form last_error at 0.2",
            RESTORE,
            |v| v["last_error"] = json!(["internal", "x", null]),
            "/last_error",
            "`last_error` of a schema_version 0.2 state must be a JSON object",
        ),
        refusal(
            "a null last_error at 0.2",
            RESTORE,
            |v| v["last_error"] = Value::Null,
            "/last_error",
            "`last_error` of a schema_version 0.2 state must be a JSON object",
        ),
        refusal(
            "an array-form singleton slot in a 0.2 state without a set",
            RESTORE,
            |v| {
                remove(v, "", "resident_set");
                let active = load_value(LEGACY)["active"].clone();
                v["active"] = json!([
                    active["deployment_id"],
                    active["bundle_digest"],
                    active["bundle_name"],
                    active["bundle_version"],
                    active["backend_hint"],
                    active["model_class"],
                    active["staged_path"]
                ]);
            },
            "/active",
            "`active` of a schema_version 0.2 state must be a JSON object",
        ),
        refusal(
            "a null in-flight transaction at 0.2",
            RESTORE,
            |v| v["in_flight_transaction"] = Value::Null,
            "/in_flight_transaction",
            "`in_flight_transaction` of a schema_version 0.2 state must be a JSON object",
        ),
        refusal(
            "a null transaction failure at 0.2",
            RESTORE,
            |v| {
                v["in_flight_transaction"] = json!({
                    "transaction_id": "tx-1", "deployment_id": "vision-v2", "phase": "failed",
                    "kind": "deploy", "failure": null
                });
            },
            "/in_flight_transaction/failure",
            "`in_flight_transaction.failure` of a schema_version 0.2 state must be a JSON object",
        ),
        refusal(
            "a null previous_active slot at 0.2",
            RESTORE,
            |v| {
                remove(v, "", "resident_set");
                v["previous_active"] = Value::Null;
            },
            "/previous_active",
            "`previous_active` of a schema_version 0.2 state must be a JSON object",
        ),
        refusal(
            "an array-form previous_active slot at 0.2",
            RESTORE,
            |v| {
                remove(v, "", "resident_set");
                let active = load_value(LEGACY)["active"].clone();
                v["previous_active"] = json!([active["deployment_id"], active["bundle_digest"]]);
            },
            "/previous_active",
            "`previous_active` of a schema_version 0.2 state must be a JSON object",
        ),
        refusal(
            "an unknown previous_active field at 0.2",
            RESTORE,
            |v| {
                remove(v, "", "resident_set");
                v["previous_active"] = load_value(LEGACY)["active"].clone();
                v["previous_active"]["later"] = json!(1);
            },
            "/previous_active",
            "unknown field `later` in `previous_active`",
        ),
        refusal(
            "a null candidate slot at 0.2",
            RESTORE,
            |v| {
                remove(v, "", "resident_set");
                v["candidate"] = Value::Null;
            },
            "/candidate",
            "`candidate` of a schema_version 0.2 state must be a JSON object",
        ),
        refusal(
            "an array-form candidate slot at 0.2",
            RESTORE,
            |v| {
                remove(v, "", "resident_set");
                let active = load_value(LEGACY)["active"].clone();
                v["candidate"] = json!([active["deployment_id"], active["bundle_digest"]]);
            },
            "/candidate",
            "`candidate` of a schema_version 0.2 state must be a JSON object",
        ),
        refusal(
            "an unknown candidate field at 0.2",
            RESTORE,
            |v| {
                remove(v, "", "resident_set");
                v["candidate"] = load_value(LEGACY)["active"].clone();
                v["candidate"]["later"] = json!(1);
            },
            "/candidate",
            "unknown field `later` in `candidate`",
        ),
        refusal(
            "a null resident set",
            TWO_MEMBER,
            |v| v["resident_set"] = Value::Null,
            RS,
            "invalid type: null",
        ),
        refusal(
            "a null counter",
            RESTORE,
            |v| {
                v["next_generation"] = Value::Null;
                remove(v, "", "resident_set");
            },
            "/next_generation",
            "invalid type: null",
        ),
        refusal(
            "a zero counter",
            RESTORE,
            |v| {
                v["next_generation"] = json!(0);
                remove(v, "", "resident_set");
            },
            "/next_generation",
            "next_generation 0 is outside",
        ),
        refusal(
            "a counter past 2^53 - 1",
            RESTORE,
            |v| v["next_generation"] = json!(9_007_199_254_740_992_u64),
            "/next_generation",
            "next_generation 9007199254740992 is outside",
        ),
        refusal(
            "an empty set id",
            TWO_MEMBER,
            |v| *at(v, "/resident_set/set_id") = json!(""),
            "/resident_set/set_id",
            "set_id must be one filesystem-safe",
        ),
        refusal(
            "a set id of `..`",
            TWO_MEMBER,
            |v| *at(v, "/resident_set/set_id") = json!(".."),
            "/resident_set/set_id",
            "set_id must be one filesystem-safe",
        ),
        refusal(
            "a set id with a slash",
            TWO_MEMBER,
            |v| *at(v, "/resident_set/set_id") = json!("a/b"),
            "/resident_set/set_id",
            "set_id must be one filesystem-safe",
        ),
        refusal(
            "a 129-byte set id",
            TWO_MEMBER,
            |v| *at(v, "/resident_set/set_id") = json!("s".repeat(129)),
            "/resident_set/set_id",
            "set_id must be one filesystem-safe",
        ),
        refusal(
            "revision zero",
            TWO_MEMBER,
            |v| *at(v, "/resident_set/revision") = json!(0),
            "/resident_set/revision",
            "revision 0 is outside",
        ),
        refusal(
            "members as an object",
            TWO_MEMBER,
            |v| *at(v, "/resident_set/members") = json!({}),
            "/resident_set/members",
            "invalid type: map",
        ),
        refusal(
            "an unknown set field",
            TWO_MEMBER,
            |v| *at(v, "/resident_set/later") = json!(1),
            RS,
            "unknown field `later`",
        ),
        refusal(
            "member generation zero",
            TWO_MEMBER,
            |v| {
                quarantine_first(v);
                *at(v, &format!("{M0}/generation")) = json!(0);
            },
            "/resident_set/members/0/generation",
            "generation 0 was never allocated",
        ),
        refusal(
            "a member id with a slash",
            TWO_MEMBER,
            |v| {
                quarantine_first(v);
                *at(v, &format!("{M0}/deployment_id")) = json!("speech/stt");
            },
            "/resident_set/members/0/deployment_id",
            "member id `speech/stt`",
        ),
        refusal(
            "a member without a descriptor digest",
            TWO_MEMBER,
            |v| remove(v, M0, "descriptor_digest"),
            "/resident_set/members/0",
            "missing field `descriptor_digest`",
        ),
        refusal(
            "a member without a configuration digest",
            TWO_MEMBER,
            |v| remove(v, M0, "configuration_digest"),
            "/resident_set/members/0",
            "missing field `configuration_digest`",
        ),
        refusal(
            "a member without a state",
            TWO_MEMBER,
            |v| remove(v, M0, "state"),
            "/resident_set/members/0",
            "missing field `state`",
        ),
        refusal(
            "a member without a staged path",
            TWO_MEMBER,
            |v| remove(v, M0, "staged_path"),
            "/resident_set/members/0",
            "missing field `staged_path`",
        ),
        refusal(
            "a malformed configuration digest",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0}/configuration_digest")) = json!("sha256:xyz"),
            "/resident_set/members/0/configuration_digest",
            "`speech-stt` configuration_digest must follow",
        ),
        refusal(
            "an uppercase digest algorithm",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0}/bundle_digest")) = json!("SHA256:00"),
            "/resident_set/members/0/bundle_digest",
            "`speech-stt` bundle_digest must follow",
        ),
        refusal(
            "an unknown admission mode",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0}/admission_mode")) = json!("shadow"),
            "/resident_set/members/0/admission_mode",
            "unknown variant `shadow`",
        ),
        refusal(
            "an unknown member state",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0}/state")) = json!("degraded"),
            "/resident_set/members/0/state",
            "unknown variant `degraded`",
        ),
        refusal(
            "a relative staged path",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0}/staged_path")) = json!("staging/speech-stt/3"),
            "/resident_set/members/0/staged_path",
            "`speech-stt` staged_path must be absolute",
        ),
        refusal(
            "an unknown member field",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0}/later")) = json!(1),
            "/resident_set/members/0",
            "unknown field `later`",
        ),
        refusal(
            "a null previous",
            TWO_MEMBER,
            |v| *at(v, PREV) = Value::Null,
            PREV,
            "invalid type: null",
        ),
        refusal(
            "an unknown retained field",
            TWO_MEMBER,
            |v| *at(v, &format!("{PREV}/later")) = json!(1),
            PREV,
            "unknown field `later`",
        ),
        refusal(
            "a retained generation with a state",
            TWO_MEMBER,
            |v| *at(v, &format!("{PREV}/state")) = json!("serving"),
            PREV,
            "unknown field `state`",
        ),
        refusal(
            "a retained generation zero",
            TWO_MEMBER,
            |v| *at(v, &format!("{PREV}/generation")) = json!(0),
            "/resident_set/members/1/previous/generation",
            "generation 0 was never allocated",
        ),
        refusal(
            "a retained id with a slash",
            TWO_MEMBER,
            |v| *at(v, &format!("{PREV}/deployment_id")) = json!("speech/tts"),
            "/resident_set/members/1/previous/deployment_id",
            "member id `speech/tts`",
        ),
        refusal(
            "a malformed retained descriptor digest",
            TWO_MEMBER,
            |v| *at(v, &format!("{PREV}/descriptor_digest")) = json!("sha256:"),
            "/resident_set/members/1/previous/descriptor_digest",
            "`speech-tts` descriptor_digest must follow",
        ),
        refusal(
            "a relative retained staged path",
            TWO_MEMBER,
            |v| *at(v, &format!("{PREV}/staged_path")) = json!("speech-tts/4"),
            "/resident_set/members/1/previous/staged_path",
            "`speech-tts` staged_path must be absolute",
        ),
        refusal(
            "a retained quota mixing shared_pool with guest_ram",
            TWO_MEMBER,
            |v| *at(v, &format!("{PREV}/quota/domain_bytes/shared_pool")) = json!(1),
            "/resident_set/members/1/previous/quota/domain_bytes",
            "must not combine shared_pool",
        ),
        refusal(
            "an unknown quota domain",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0Q}/domain_bytes/host_ram")) = json!(1),
            "/resident_set/members/0/quota/domain_bytes",
            "unknown field `host_ram`",
        ),
        refusal(
            "quota bytes past 2^53 - 1",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0Q}/domain_bytes/guest_ram")) = json!(9_007_199_254_740_992_u64),
            "/resident_set/members/0/quota/domain_bytes/guest_ram",
            "byte values must be integers",
        ),
        refusal(
            "negative quota bytes",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0Q}/domain_bytes/guest_ram")) = json!(-1),
            "/resident_set/members/0/quota/domain_bytes/guest_ram",
            "byte values must be integers",
        ),
        refusal(
            "fractional quota bytes",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0Q}/domain_bytes/guest_ram")) = json!(1.5),
            "/resident_set/members/0/quota/domain_bytes/guest_ram",
            "byte values must be integers",
        ),
        refusal(
            "domain bytes as an array",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0Q}/domain_bytes")) = json!([1, 2]),
            "/resident_set/members/0/quota/domain_bytes",
            "invalid type: sequence, expected a JSON object",
        ),
        refusal(
            "a null quota domain",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0Q}/domain_bytes/guest_ram")) = Value::Null,
            "/resident_set/members/0/quota/domain_bytes/guest_ram",
            "invalid type: null",
        ),
        refusal(
            "a session count past the member bound",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0Q}/session_count")) = json!(2_049),
            "/resident_set/members/0/quota/session_count",
            "session_count 2049 exceeds the 2048 sessions",
        ),
        refusal(
            "a session count past u32",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0Q}/session_count")) = json!(4_294_967_296_u64),
            "/resident_set/members/0/quota/session_count",
            "invalid value: integer `4294967296`",
        ),
        refusal(
            "a negative session count",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0Q}/session_count")) = json!(-1),
            "/resident_set/members/0/quota/session_count",
            "invalid value: integer `-1`",
        ),
        refusal(
            "sessions with no domain bytes",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0Q}/domain_bytes")) = json!({}),
            "/resident_set/members/0/quota/domain_bytes",
            "session_count is positive",
        ),
        refusal(
            "shared_pool beside guest_ram",
            TWO_MEMBER,
            |v| *at(v, &format!("{M0Q}/domain_bytes")) = json!({"shared_pool": 1, "guest_ram": 1}),
            "/resident_set/members/0/quota/domain_bytes",
            "must not combine shared_pool",
        ),
        refusal(
            "shared_pool beside device_vram",
            TWO_MEMBER,
            |v| {
                *at(v, &format!("{M0Q}/domain_bytes")) =
                    json!({"shared_pool": 1, "device_vram": 1});
            },
            "/resident_set/members/0/quota/domain_bytes",
            "must not combine shared_pool",
        ),
        refusal(
            "an endpoint entry with no endpoint",
            TWO_MEMBER,
            |v| remove(v, E0, "stream_endpoint"),
            E0,
            "names neither a unary nor a stream endpoint",
        ),
        refusal(
            "an empty endpoint",
            TWO_MEMBER,
            |v| *at(v, &format!("{E0}/stream_endpoint")) = json!(""),
            "/resident_set/endpoint_map/0/stream_endpoint",
            "has an empty endpoint",
        ),
        refusal(
            "an unknown endpoint field",
            TWO_MEMBER,
            |v| *at(v, &format!("{E0}/later")) = json!(1),
            E0,
            "unknown field `later`",
        ),
        refusal(
            "an endpoint id with a slash",
            TWO_MEMBER,
            |v| *at(v, &format!("{E0}/deployment_id")) = json!("speech/stt"),
            "/resident_set/endpoint_map/0/deployment_id",
            "entry speech/stt@3 does not match",
        ),
        refusal(
            "an endpoint at generation zero",
            TWO_MEMBER,
            |v| *at(v, &format!("{E0}/generation")) = json!(0),
            "/resident_set/endpoint_map/0/generation",
            "entry speech-stt@0 does not match",
        ),
    ]
}

#[test]
fn documents_both_refuse() {
    for case in refused_by_both() {
        let Refusal {
            label,
            base,
            edit,
            schema_at,
            decoder,
        } = case;
        let doc = edited(base, edit);
        let paths: BTreeSet<String> = match validator().validate(&doc) {
            Ok(()) => BTreeSet::new(),
            Err(errors) => errors.map(|e| e.instance_path.to_string()).collect(),
        };
        // Exactly one location: a case that breaks two rules could pass
        // for the one it does not name.
        assert_eq!(
            paths,
            BTreeSet::from([schema_at.to_string()]),
            "{label}: the schema's errors are not all at `{schema_at}`"
        );
        match decode(&doc) {
            Ok(_) => panic!("{label}: the decoder accepted it"),
            Err(err) => {
                let message = err.to_string();
                assert!(
                    message.contains(decoder),
                    "{label}: expected the decoder to say `{decoder}`, got `{message}`"
                );
            }
        }
    }
}

#[test]
fn documents_both_accept() {
    let cases: Vec<(&str, &str, Edit)> = vec![
        ("the fixture unchanged", TWO_MEMBER, |_| {}),
        ("a member without a previous generation", TWO_MEMBER, |v| {
            remove(v, M1, "previous");
        }),
        ("a quarantined member with no endpoint", TWO_MEMBER, |v| {
            *at(v, &format!("{M0}/state")) = json!("quarantined");
            at(v, "/resident_set/endpoint_map")
                .as_array_mut()
                .expect("array")
                .remove(0);
        }),
        ("a member in qualification mode", TWO_MEMBER, |v| {
            *at(v, &format!("{M0}/admission_mode")) = json!("qualification");
            *at(v, &format!("{M0}/quota/session_count")) = json!(4);
        }),
        ("a unified-memory quota", TWO_MEMBER, |v| {
            *at(v, &format!("{M0}/quota/domain_bytes")) = json!({"shared_pool": 1});
        }),
        ("a member that serves no sessions", TWO_MEMBER, |v| {
            *at(v, &format!("{M0}/quota")) = json!({"session_count": 0, "domain_bytes": {}});
        }),
        ("a unary and a stream endpoint", TWO_MEMBER, |v| {
            *at(v, "/resident_set/endpoint_map/0/unary_endpoint") = json!("http://127.0.0.1:18080");
        }),
        ("an empty set", TWO_MEMBER, |v| {
            *at(v, "/resident_set/members") = json!([]);
            *at(v, "/resident_set/endpoint_map") = json!([]);
        }),
        ("a 0.2 state carrying only the counter", RESTORE, |v| {
            remove(v, "", "resident_set");
        }),
        ("a 0.1 state with a 129-byte singleton id", LEGACY, |v| {
            v["active"]["deployment_id"] = json!("d".repeat(129));
        }),
        ("labels on a member", TWO_MEMBER, |v| {
            *at(v, &format!("{M0}/labels")) = json!({"team": "speech"});
        }),
    ];
    for (label, base, edit) in cases {
        let doc = edited(base, edit);
        let errors = schema_errors(&doc);
        assert!(
            errors.is_empty(),
            "{label}: the schema refused it: {errors:?}"
        );
        decode(&doc).unwrap_or_else(|e| panic!("{label}: the decoder refused it: {e}"));
    }
}

/// Rules Draft-07 cannot state across array elements, enforced by the
/// decoder alone: each case is schema-valid and refused with its own
/// message.
#[test]
fn the_decoder_enforces_the_cross_record_rules() {
    let cases: Vec<(&str, Edit, &str)> = vec![
        (
            "a duplicate member id at a fresh generation",
            |v| {
                *at(v, &format!("{M1}/deployment_id")) = json!("speech-stt");
                *at(v, "/resident_set/endpoint_map/1/deployment_id") = json!("speech-stt");
            },
            "resident set member `speech-stt` appears more than once",
        ),
        (
            "a duplicate generation under a fresh id",
            |v| {
                *at(v, &format!("{M1}/generation")) = json!(3);
                *at(v, "/resident_set/endpoint_map/1/generation") = json!(3);
                remove(v, M1, "previous");
            },
            "generation 3 appears more than once in the resident set",
        ),
        (
            "a previous generation reused by another member",
            |v| *at(v, &format!("{M1}/previous/generation")) = json!(3),
            "generation 3 appears more than once in the resident set",
        ),
        (
            "a generation the counter never issued",
            |v| v["next_generation"] = json!(5),
            "`speech-tts` generation 5 was never allocated (next_generation is 5)",
        ),
        (
            "a previous generation newer than its member",
            |v| {
                *at(v, &format!("{M1}/previous/generation")) = json!(5);
                *at(v, &format!("{M1}/generation")) = json!(4);
                *at(v, "/resident_set/endpoint_map/1/generation") = json!(4);
            },
            "`speech-tts` previous generation 5 is not older than generation 4",
        ),
        (
            "an endpoint at the wrong generation",
            |v| *at(v, "/resident_set/endpoint_map/1/generation") = json!(4),
            "endpoint_map entry speech-tts@4 does not match the next serving member speech-tts@5",
        ),
        (
            "endpoints out of member order",
            |v| {
                let map = at(v, "/resident_set/endpoint_map")
                    .as_array_mut()
                    .expect("array");
                map.swap(0, 1);
            },
            "endpoint_map entry speech-tts@5 does not match the next serving member speech-stt@3",
        ),
        (
            "an endpoint for a quarantined member",
            |v| *at(v, &format!("{M0}/state")) = json!("quarantined"),
            "endpoint_map entry speech-stt@3 does not match the next serving member speech-tts@5",
        ),
        (
            "a serving member with no endpoint",
            |v| {
                at(v, "/resident_set/endpoint_map")
                    .as_array_mut()
                    .expect("array")
                    .pop();
            },
            "serving member `speech-tts` has no endpoint_map entry",
        ),
        (
            "an endpoint with no serving member left",
            |v| {
                let extra = json!({"deployment_id": "speech-extra", "generation": 2, "stream_endpoint": "127.0.0.1:1"});
                at(v, "/resident_set/endpoint_map")
                    .as_array_mut()
                    .expect("array")
                    .push(extra);
            },
            "endpoint_map entry `speech-extra` has no serving member left to describe",
        ),
    ];
    for (label, edit, message) in cases {
        let doc = edited(TWO_MEMBER, edit);
        let errors = schema_errors(&doc);
        assert!(
            errors.is_empty(),
            "{label}: the schema refused it: {errors:?}"
        );
        match decode(&doc) {
            Err(DecodeError::InvalidPayload(got)) => assert!(
                got.ends_with(message),
                "{label}: expected `{message}`, got `{got}`"
            ),
            other => panic!("{label}: expected InvalidPayload, got {other:?}"),
        }
    }
}

/// Where the two readers knowingly disagree.
#[test]
fn documented_divergences() {
    // Draft-07 `type: integer` accepts an integral float; the decoder's
    // counters are exact integers.
    let doc = edited(TWO_MEMBER, |v| {
        *at(v, "/resident_set/revision") = json!(4.0);
    });
    assert!(schema_errors(&doc).is_empty());
    assert!(matches!(decode(&doc), Err(DecodeError::Malformed(_))));

    // A key repeated inside a record: the schema only ever sees the last
    // value, the decoder refuses the document.
    let raw = load(TWO_MEMBER).replacen("\"revision\": 4,", "\"revision\": 3, \"revision\": 4,", 1);
    assert_ne!(raw, load(TWO_MEMBER));
    assert!(schema_errors(&serde_json::from_str(&raw).expect("parses")).is_empty());
    assert!(matches!(
        decode_agent_state(&raw),
        Err(DecodeError::Malformed(_))
    ));

    // A 0.1 file keeps the lenient reading every earlier agent applied: an
    // unknown field, at the top level or in a record, is ignored by the
    // decoder and refused by the schema.
    let edits: [Edit; 3] = [
        |v| v["later"] = json!(true),
        |v| v["last_error"]["later"] = json!(true),
        |v| v["quarantined"][0]["error"]["later"] = json!(true),
    ];
    for edit in edits {
        let doc = edited(LEGACY, edit);
        assert!(!schema_errors(&doc).is_empty());
        assert_eq!(
            decode(&doc).expect("lenient"),
            decode_agent_state(&load(LEGACY)).expect("legacy")
        );
    }
}

/// Every error code, phase and transaction kind. Adding a variant fails to
/// compile in these matches until they handle it; the lists beside them
/// must gain it too.
fn all_error_codes() -> Vec<ErrorCode> {
    fn exhaustive(code: ErrorCode) {
        match code {
            ErrorCode::ConfigInvalid
            | ErrorCode::LoadFailed
            | ErrorCode::NotReady
            | ErrorCode::ShapeMismatch
            | ErrorCode::Unsupported
            | ErrorCode::OomError
            | ErrorCode::Timeout
            | ErrorCode::InferenceFailed
            | ErrorCode::Internal
            | ErrorCode::Cancelled
            | ErrorCode::Unavailable
            | ErrorCode::ResourceExhausted => {}
        }
    }
    let all = vec![
        ErrorCode::ConfigInvalid,
        ErrorCode::LoadFailed,
        ErrorCode::NotReady,
        ErrorCode::ShapeMismatch,
        ErrorCode::Unsupported,
        ErrorCode::OomError,
        ErrorCode::Timeout,
        ErrorCode::InferenceFailed,
        ErrorCode::Internal,
        ErrorCode::Cancelled,
        ErrorCode::Unavailable,
        ErrorCode::ResourceExhausted,
    ];
    all.iter().copied().for_each(exhaustive);
    all
}

fn all_phases() -> Vec<DeployState> {
    fn exhaustive(phase: DeployState) {
        match phase {
            DeployState::Received
            | DeployState::Verified
            | DeployState::Staged
            | DeployState::CapacityChecked
            | DeployState::Prepared
            | DeployState::Warmed
            | DeployState::Promoted
            | DeployState::Active
            | DeployState::Failed
            | DeployState::RolledBack => {}
        }
    }
    let all = vec![
        DeployState::Received,
        DeployState::Verified,
        DeployState::Staged,
        DeployState::CapacityChecked,
        DeployState::Prepared,
        DeployState::Warmed,
        DeployState::Promoted,
        DeployState::Active,
        DeployState::Failed,
        DeployState::RolledBack,
    ];
    all.iter().copied().for_each(exhaustive);
    all
}

fn all_kinds() -> Vec<TransactionKind> {
    fn exhaustive(kind: TransactionKind) {
        match kind {
            TransactionKind::Deploy | TransactionKind::Rollback => {}
        }
    }
    let all = vec![TransactionKind::Deploy, TransactionKind::Rollback];
    all.iter().copied().for_each(exhaustive);
    all
}

fn spelled<T: serde::Serialize>(values: &[T]) -> Vec<Value> {
    values
        .iter()
        .map(|v| serde_json::to_value(v).expect("spelling"))
        .collect()
}

/// The schema's 0.1 branch pins exactly the vocabulary the decoder treats
/// as readable by agents through 0.2.x: a value is in the pinned list if
/// and only if a 0.1 state carrying it still needs only state version 0.1.
#[test]
fn the_0_1_vocabulary_pins_match_the_decoders_classification() {
    let schema = schema_document();
    let defs = &schema["definitions"];
    let pinned_codes = defs["LegacyErrorRecord"]["properties"]["code"]["enum"]
        .as_array()
        .expect("codes")
        .clone();
    let pinned_phases = defs["LegacyPhase"]["enum"]
        .as_array()
        .expect("phases")
        .clone();
    let pinned_kinds = schema["allOf"][0]["then"]["properties"]["in_flight_transaction"]
        ["properties"]["kind"]["enum"]
        .as_array()
        .expect("kinds")
        .clone();

    let legacy =
        |state: &AgentState| state.required_schema_version() == AGENT_STATE_SCHEMA_VERSION_LEGACY;
    let base = decode_agent_state(&load(LEGACY)).expect("legacy");
    let codes = all_error_codes();
    let expected_codes: Vec<Value> = spelled(&codes)
        .into_iter()
        .zip(&codes)
        .filter(|(_, code)| {
            let mut s = base.clone();
            s.last_error.as_mut().expect("last_error").code = **code;
            legacy(&s)
        })
        .map(|(spelling, _)| spelling)
        .collect();
    assert_eq!(pinned_codes, expected_codes);

    let phases = all_phases();
    let expected_phases: Vec<Value> = spelled(&phases)
        .into_iter()
        .zip(&phases)
        .filter(|(_, phase)| {
            let mut s = base.clone();
            s.quarantined[0].phase = **phase;
            legacy(&s)
        })
        .map(|(spelling, _)| spelling)
        .collect();
    assert_eq!(pinned_phases, expected_phases);

    let kinds = all_kinds();
    assert_eq!(pinned_kinds, spelled(&kinds));

    // Every pinned list is allowed by the shared record definitions. The
    // shared error record admits every code; the pinned list is the prefix
    // agents through 0.2.x decode, and the codes appended after the 0.2.1
    // state writer shipped (cancelled, unavailable, resource_exhausted) are
    // "0.2" content, so a 0.1 file may carry only the pinned nine.
    let shared_codes = defs["ErrorRecord"]["properties"]["code"]["enum"]
        .as_array()
        .expect("error record codes")
        .clone();
    assert_eq!(shared_codes, spelled(&codes));
    assert!(
        shared_codes.starts_with(&pinned_codes),
        "the pinned 0.1 codes must be a prefix of the shared error record's codes"
    );
    assert_eq!(
        pinned_phases,
        *defs["TransactionRecord"]["properties"]["phase"]["enum"]
            .as_array()
            .expect("transaction phases")
    );
    assert_eq!(
        pinned_kinds,
        *defs["TransactionRecord"]["properties"]["kind"]["enum"]
            .as_array()
            .expect("transaction kinds")
    );
}
