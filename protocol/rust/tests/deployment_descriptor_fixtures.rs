// SPDX-License-Identifier: Apache-2.0
//
// Deployment descriptor and canonical JSON fixtures, replayed against the schema and the reader.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::OnceLock;

use serde_json::{json, Value};
use tensorplate_protocol::backend_descriptor::{BackendDescriptor, RunnerProfile};
use tensorplate_protocol::canonical_json::{canonicalize, sha256_digest};
use tensorplate_protocol::{
    parse_bundle, AdmissionMode, DeploymentDescriptor, DeploymentDescriptorError, DescriptorInputs,
    DomainQuotaBytes, ErrorCode, MemberQuota, ProtocolError, CANONICAL_JSON_VERSION,
    MEMORY_BUDGET_LINE_NAMES,
};

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn read(relative: &str) -> String {
    let path = repo_path(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn read_json(relative: &str) -> Value {
    serde_json::from_str(&read(relative)).unwrap_or_else(|e| panic!("{relative}: {e}"))
}

const STT: &str = "protocol/fixtures/deployment_descriptor_stt.json";
const STT_RESTART: &str = "protocol/fixtures/deployment_descriptor_stt_restart.json";
const TTS: &str = "protocol/fixtures/deployment_descriptor_tts.json";

fn validator() -> &'static jsonschema::JSONSchema {
    static VALIDATOR: OnceLock<jsonschema::JSONSchema> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let mut options = jsonschema::JSONSchema::options();
        for relative in [
            "protocol/schemas/bundle_manifest.json",
            "config/schemas/memory_budget_breakdown.json",
            "protocol/schemas/backend_descriptor.json",
            "protocol/schemas/worker_control.json",
        ] {
            let doc = read_json(relative);
            let id = doc["$id"].as_str().expect("$id").to_owned();
            options.with_document(id, doc);
        }
        options
            .compile(&read_json("protocol/schemas/deployment_descriptor.json"))
            .expect("schema compiles as Draft-07")
    })
}

fn installed_runner_profiles() -> Vec<RunnerProfile> {
    let relative = "protocol/rust/tests/fixtures/backend_descriptor_runner_profiles.json";
    BackendDescriptor::parse_with_path(&read(relative), &repo_path(relative))
        .expect("backend descriptor fixture")
        .runner_profiles
}

fn quota(session_count: u32, guest_ram: u64, device_vram: u64) -> MemberQuota {
    MemberQuota {
        session_count,
        domain_bytes: DomainQuotaBytes {
            guest_ram: Some(guest_ram),
            device_vram: Some(device_vram),
            ..DomainQuotaBytes::default()
        },
    }
}

/// The inputs each committed descriptor was produced from.
struct Case {
    bundle: &'static str,
    deployment_id: &'static str,
    generation: u64,
    admission_mode: AdmissionMode,
    quota: MemberQuota,
}

fn case(fixture: &str) -> Case {
    match fixture {
        STT | STT_RESTART => Case {
            bundle: "test/models/bundles/v0_2/speech_stt_streaming",
            deployment_id: "speech-stt",
            generation: if fixture == STT { 3 } else { 4 },
            admission_mode: AdmissionMode::Qualification,
            quota: quota(1, 33_554_432, 67_108_864),
        },
        TTS => Case {
            bundle: "test/models/bundles/v0_2/speech_tts_streaming",
            deployment_id: "speech-tts",
            generation: 5,
            admission_mode: AdmissionMode::Production,
            quota: quota(1, 16_777_216, 33_554_432),
        },
        other => panic!("no inputs recorded for {other}"),
    }
}

fn derive_with(
    fixture: &str,
    profiles: &[RunnerProfile],
    edit: impl FnOnce(&mut DescriptorInputs<'_>),
) -> Result<DeploymentDescriptor, DeploymentDescriptorError> {
    let case = case(fixture);
    let bundle = parse_bundle(&repo_path(case.bundle)).expect("committed bundle parses");
    let staged_path = format!(
        "/var/lib/tensorplate/bundles/staging/{}/{}",
        case.deployment_id, case.generation
    );
    let mut inputs = DescriptorInputs {
        deployment_id: case.deployment_id,
        generation: case.generation,
        admission_mode: case.admission_mode,
        staged_path: &staged_path,
        quota: case.quota,
        runtime_version: "0.3.1",
        runner_profiles: profiles,
        acceptance_profile_digest: None,
    };
    edit(&mut inputs);
    DeploymentDescriptor::derive(&bundle, &inputs)
}

fn derive(fixture: &str) -> DeploymentDescriptor {
    derive_with(fixture, &installed_runner_profiles(), |_| {}).expect("derive")
}

#[test]
fn every_canonical_json_vector_replays() {
    let vectors = read_json("protocol/fixtures/canonical_json.json");
    assert_eq!(
        vectors["canonical_json_version"].as_u64(),
        Some(CANONICAL_JSON_VERSION)
    );
    let accept = vectors["accept"].as_array().expect("accept");
    let reject = vectors["reject"].as_array().expect("reject");
    assert!(!accept.is_empty() && !reject.is_empty());
    for vector in accept {
        let name = vector["name"].as_str().expect("name");
        let input = vector["input"].as_str().expect("input");
        let expected = vector["canonical"].as_str().expect("canonical").as_bytes();
        let got = canonicalize(input).unwrap_or_else(|e| panic!("{name}: refused: {e}"));
        assert!(
            got == expected,
            "{name}: got {:?} ({} bytes), want {:?} ({} bytes)",
            String::from_utf8_lossy(&got),
            got.len(),
            String::from_utf8_lossy(expected),
            expected.len()
        );
        assert_eq!(sha256_digest(&got), vector["sha256"], "{name}");
        let again = canonicalize(vector["canonical"].as_str().expect("canonical"));
        assert_eq!(again.as_deref(), Ok(expected), "{name}: not idempotent");
    }
    let reasons = vectors["reasons"].as_object().expect("reasons");
    let mut exercised = std::collections::BTreeSet::new();
    for vector in reject {
        let name = vector["name"].as_str().expect("name");
        let reason = vector["reason"].as_str().expect("reason");
        assert!(reasons.contains_key(reason), "{name}: unlisted reason");
        let err = canonicalize(vector["input"].as_str().expect("input"))
            .expect_err(&format!("{name}: accepted"));
        assert_eq!(err.reason(), reason, "{name}: refused with `{err}`");
        exercised.insert(reason);
    }
    assert_eq!(
        exercised.len(),
        reasons.len(),
        "a listed reason has no vector"
    );
}

#[test]
fn the_standard_library_reference_writes_the_committed_fixtures() {
    let status = std::process::Command::new("python3")
        .arg(repo_path("protocol/fixtures/canonical_json_reference.py"))
        .arg("--check")
        .status()
        .expect("python3 runs the reference implementation");
    assert!(status.success(), "the reference writes different fixtures");
}

#[test]
fn committed_descriptors_validate_and_read_back_verified() {
    for fixture in [STT, STT_RESTART, TTS] {
        let text = read(fixture);
        let doc: Value = serde_json::from_str(&text).expect("JSON");
        assert!(validator().is_valid(&doc), "{fixture} must validate");
        let descriptor = DeploymentDescriptor::from_json(&text)
            .unwrap_or_else(|e| panic!("{fixture} must read back: {e}"));
        assert_eq!(serde_json::to_value(&descriptor).expect("serialize"), doc);
        assert_eq!(descriptor.configuration_digest, doc["configuration_digest"]);
        assert_eq!(descriptor.descriptor_digest, doc["descriptor_digest"]);
    }
}

#[test]
fn committed_descriptors_are_what_the_speech_bundles_derive() {
    for fixture in [STT, STT_RESTART, TTS] {
        let expected = DeploymentDescriptor::from_json(&read(fixture)).expect("fixture");
        assert_eq!(derive(fixture), expected, "{fixture}");
    }
}

#[test]
fn an_equivalent_restart_keeps_the_configuration_and_changes_the_instance() {
    let first = derive(STT);
    let restart = derive(STT_RESTART);
    assert_eq!(first.configuration, restart.configuration);
    assert_eq!(first.configuration_digest, restart.configuration_digest);
    assert_ne!(first.generation, restart.generation);
    assert_ne!(first.descriptor_digest, restart.descriptor_digest);
    assert_eq!(first.member().deployment_id, restart.member().deployment_id);
}

type InputsEdit = fn(&mut DescriptorInputs<'_>);

#[test]
fn only_configuration_inputs_move_the_configuration_digest() {
    let base = derive(STT);
    let configuration_changes: [(&str, InputsEdit); 3] = [
        ("session count", |i| i.quota.session_count = 2),
        ("runtime version", |i| i.runtime_version = "0.3.2"),
        ("acceptance profile", |i| {
            i.acceptance_profile_digest =
                Some("sha256:0000000000000000000000000000000000000000000000000000000000000001");
        }),
    ];
    for (name, edit) in configuration_changes {
        let changed = derive_with(STT, &installed_runner_profiles(), edit).expect(name);
        assert_ne!(
            changed.configuration_digest, base.configuration_digest,
            "{name}"
        );
        assert_ne!(changed.descriptor_digest, base.descriptor_digest, "{name}");
    }
    let instance_changes: [(&str, InputsEdit); 3] = [
        ("generation", |i| i.generation = 9),
        ("admission mode", |i| {
            i.admission_mode = AdmissionMode::Production;
        }),
        ("staged root", |i| {
            i.staged_path = "/srv/staging/speech-stt/3";
        }),
    ];
    for (name, edit) in instance_changes {
        let changed = derive_with(STT, &installed_runner_profiles(), edit).expect(name);
        assert_eq!(
            changed.configuration_digest, base.configuration_digest,
            "{name}"
        );
        assert_ne!(changed.descriptor_digest, base.descriptor_digest, "{name}");
    }
}

#[test]
fn the_configuration_holds_no_instance_digest_or_evidence_field() {
    for fixture in [STT, TTS] {
        let doc = read_json(fixture);
        let configuration = doc["configuration"].as_object().expect("configuration");
        for key in [
            "deployment_id",
            "generation",
            "admission_mode",
            "staged_path",
            "runner_environment",
            "configuration_digest",
            "descriptor_digest",
        ] {
            assert!(!configuration.contains_key(key), "{fixture}: {key}");
        }
        assert!(
            !read(fixture).contains("evidence"),
            "{fixture} names an evidence record"
        );
    }
}

#[test]
fn a_format_0_1_bundle_derives_a_descriptor_without_profile_fields() {
    let bundle =
        parse_bundle(&repo_path("test/models/bundles/v0_1/vision_tensorrt")).expect("bundle");
    let descriptor = DeploymentDescriptor::derive(
        &bundle,
        &DescriptorInputs {
            deployment_id: "vision",
            generation: 1,
            admission_mode: AdmissionMode::Production,
            staged_path: "/var/lib/tensorplate/bundles/staging/vision/1",
            quota: MemberQuota::default(),
            runtime_version: "0.3.1",
            runner_profiles: &[],
            acceptance_profile_digest: None,
        },
    )
    .expect("derive");
    let doc = serde_json::to_value(&descriptor).expect("serialize");
    assert!(validator().is_valid(&doc));
    assert_eq!(doc["configuration"]["bundle"]["format_version"], "0.1");
    for key in [
        "runner_profile",
        "compute_type",
        "speech",
        "pipeline_stages",
    ] {
        assert!(doc["configuration"].get(key).is_none(), "{key}");
    }
    assert!(doc.get("runner_environment").is_none());
    let text = serde_json::to_string(&doc).expect("text");
    assert_eq!(
        DeploymentDescriptor::from_json(&text).expect("read"),
        descriptor
    );
}

#[test]
fn derivation_refuses_a_runner_it_cannot_resolve() {
    let err = derive_with(STT, &[], |_| {}).expect_err("no profiles");
    assert!(
        matches!(&err, DeploymentDescriptorError::UnknownRunnerProfile(id) if id == "faster_whisper"),
        "{err}"
    );
    let mut profiles = installed_runner_profiles();
    for profile in &mut profiles {
        profile
            .compute_types
            .retain(|c| serde_json::to_value(c).expect("compute type") != json!("float16"));
    }
    let err = derive_with(STT, &installed_runner_profiles(), |i| {
        i.generation = 1 << 53;
    })
    .expect_err("generation beyond 2^53-1");
    assert_eq!(refusal_of(&err), "Invalid(generation)", "{err}");
    let err = derive_with(STT, &profiles, |_| {}).expect_err("no float16");
    assert!(
        matches!(&err, DeploymentDescriptorError::UnsupportedComputeType { compute_type, .. } if compute_type == "float16"),
        "{err}"
    );
}

/// How the reader refused, in the form the cases below name.
fn refusal_of(err: &DeploymentDescriptorError) -> String {
    match err {
        DeploymentDescriptorError::Canonical(e) => format!("Canonical({})", e.reason()),
        DeploymentDescriptorError::Malformed(_) => "Malformed".into(),
        DeploymentDescriptorError::UnsupportedSchemaVersion(_) => "SchemaVersion".into(),
        DeploymentDescriptorError::UnsupportedCanonicalJsonVersion(_) => "CanonicalVersion".into(),
        DeploymentDescriptorError::Profile(_) => "Profile".into(),
        DeploymentDescriptorError::Invalid { field, .. } => format!("Invalid({field})"),
        DeploymentDescriptorError::NotNormalized => "NotNormalized".into(),
        DeploymentDescriptorError::ConfigurationDigestMismatch { .. } => {
            "ConfigurationDigest".into()
        }
        DeploymentDescriptorError::DescriptorDigestMismatch { .. } => "DescriptorDigest".into(),
        other => format!("{other:?}"),
    }
}

fn set(pointer: &str, value: Value) -> impl Fn(&mut Value) + '_ {
    move |doc: &mut Value| *doc.pointer_mut(pointer).expect(pointer) = value.clone()
}

fn remove(pointer: &str) -> impl Fn(&mut Value) + '_ {
    move |doc: &mut Value| {
        let (parent, key) = pointer.rsplit_once('/').expect("pointer");
        doc.pointer_mut(parent)
            .and_then(Value::as_object_mut)
            .expect(parent)
            .remove(key)
            .expect(pointer);
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn the_reader_refuses_each_document_for_its_own_reason() {
    type Edit = Box<dyn Fn(&mut Value)>;
    let cases: Vec<(&str, &str, Edit, &str, bool)> = vec![
        (
            "quota edited, digests kept",
            STT,
            Box::new(set("/configuration/quota/session_count", json!(2))),
            "ConfigurationDigest",
            true,
        ),
        (
            "generation edited, digests kept",
            STT,
            Box::new(set("/generation", json!(9))),
            "DescriptorDigest",
            true,
        ),
        (
            "instance digest from the restart",
            STT,
            Box::new(set(
                "/descriptor_digest",
                read_json(STT_RESTART)["descriptor_digest"].clone(),
            )),
            "DescriptorDigest",
            true,
        ),
        (
            "null member",
            STT,
            Box::new(|doc: &mut Value| {
                doc["configuration"]["acceptance_profile_digest"] = Value::Null;
            }),
            "Canonical(null)",
            false,
        ),
        (
            "fractional count",
            STT,
            Box::new(set("/generation", json!(3.0))),
            "Canonical(number)",
            true,
        ),
        (
            "schema version 0.2",
            STT,
            Box::new(set("/schema_version", json!("0.2"))),
            "SchemaVersion",
            false,
        ),
        (
            "canonical JSON version 2",
            STT,
            Box::new(set("/canonical_json_version", json!(2))),
            "CanonicalVersion",
            false,
        ),
        (
            "unknown field",
            STT,
            Box::new(|doc: &mut Value| doc["configuration"]["runtime"] = json!("x")),
            "Malformed",
            false,
        ),
        (
            "budget line left out",
            STT,
            Box::new(remove(
                "/configuration/memory_budget_by_domain/guest_ram/cache_bytes",
            )),
            "NotNormalized",
            false,
        ),
        (
            "observable left out",
            STT,
            Box::new(remove("/configuration/pipeline_stages/0/observable")),
            "NotNormalized",
            false,
        ),
        (
            "bundle written as an array",
            STT,
            Box::new(|doc: &mut Value| {
                let b = doc["configuration"]["bundle"].clone();
                doc["configuration"]["bundle"] = json!([
                    b["name"],
                    b["version"],
                    b["format_version"],
                    b["bundle_digest"]
                ]);
            }),
            "NotNormalized",
            false,
        ),
        (
            "speech frame below 20 ms",
            STT,
            Box::new(set(
                "/configuration/speech/chunking/frame_ms_min",
                json!(10),
            )),
            "Profile",
            false,
        ),
        (
            "runner environment left out",
            STT,
            Box::new(remove("/runner_environment")),
            "Invalid(runner_environment)",
            true,
        ),
        (
            "format 0.2 fields on format 0.1",
            STT,
            Box::new(set("/configuration/bundle/format_version", json!("0.1"))),
            "Invalid(configuration)",
            true,
        ),
        (
            "second model artifact",
            STT,
            Box::new(|doc: &mut Value| {
                let mut extra = doc["configuration"]["artifacts"][0].clone();
                extra["path"] = json!("other.json");
                doc["configuration"]["artifacts"]
                    .as_array_mut()
                    .expect("artifacts")
                    .push(extra);
            }),
            "Invalid(configuration.artifacts)",
            true,
        ),
        (
            "artifact outside the root",
            STT,
            Box::new(set(
                "/configuration/artifacts/0/path",
                json!("../entry.json"),
            )),
            "Invalid(configuration.artifacts)",
            true,
        ),
        (
            "uppercase artifact digest",
            STT,
            Box::new(|doc: &mut Value| {
                let digest = doc["configuration"]["artifacts"][0]["digest"]
                    .as_str()
                    .expect("digest");
                let upper = format!(
                    "sha256:{}",
                    digest.trim_start_matches("sha256:").to_ascii_uppercase()
                );
                doc["configuration"]["artifacts"][0]["digest"] = json!(upper);
            }),
            "Invalid(configuration.artifacts)",
            false,
        ),
        (
            "warmup fixture not an artifact",
            TTS,
            Box::new(set(
                "/configuration/warmup/fixtures/0",
                json!("warmup/other.txt"),
            )),
            "Invalid(configuration.warmup.fixtures)",
            true,
        ),
        (
            "dot-dot member name",
            STT,
            Box::new(set("/deployment_id", json!(".."))),
            "Invalid(deployment_id)",
            false,
        ),
        (
            "generation zero",
            STT,
            Box::new(set("/generation", json!(0))),
            "Invalid(generation)",
            false,
        ),
        (
            "relative staged root",
            STT,
            Box::new(set("/staged_path", json!("staging/speech-stt/3"))),
            "Invalid(staged_path)",
            false,
        ),
        (
            "runtime version with a space",
            STT,
            Box::new(set("/configuration/runtime_version", json!("0.3.1 rc"))),
            "Invalid(configuration.runtime_version)",
            false,
        ),
        (
            "sessions without quota bytes",
            STT,
            Box::new(set("/configuration/quota/domain_bytes", json!({}))),
            "Invalid(configuration.quota)",
            false,
        ),
        (
            "speech contract on a vision bundle",
            STT,
            Box::new(set("/configuration/model_class", json!("vision"))),
            "Invalid(configuration.speech)",
            true,
        ),
        (
            "unrecognized backend",
            STT,
            Box::new(set("/configuration/backend_hint", json!("not_a_backend"))),
            "Invalid(configuration.backend_hint)",
            true,
        ),
        (
            "empty bundle name",
            STT,
            Box::new(set("/configuration/bundle/name", json!(""))),
            "Invalid(configuration.bundle)",
            false,
        ),
        (
            "format version not MAJOR.MINOR",
            STT,
            Box::new(set("/configuration/bundle/format_version", json!("0.x"))),
            "Invalid(configuration.bundle.format_version)",
            false,
        ),
        (
            "bundle digest not sha256",
            STT,
            Box::new(set(
                "/configuration/bundle/bundle_digest",
                json!("sha256:xyz"),
            )),
            "Invalid(configuration.bundle.bundle_digest)",
            false,
        ),
        (
            "artifact path listed twice",
            TTS,
            Box::new(set("/configuration/artifacts/1/path", json!("entry.json"))),
            "Invalid(configuration.artifacts)",
            true,
        ),
        (
            "no runner packages",
            STT,
            Box::new(set("/configuration/runner_profile/packages", json!([]))),
            "Invalid(configuration.runner_profile.packages)",
            false,
        ),
        (
            "runner package listed twice",
            STT,
            Box::new(set(
                "/configuration/runner_profile/packages/1",
                json!("tensorplate-speech-runtime-base"),
            )),
            "Invalid(configuration.runner_profile.packages)",
            false,
        ),
        (
            "interpreter outside the environment",
            STT,
            Box::new(set(
                "/runner_environment/interpreter",
                json!("/usr/bin/python3"),
            )),
            "Invalid(runner_environment)",
            true,
        ),
        (
            "library path climbing out of the environment",
            STT,
            Box::new(set(
                "/runner_environment/library_search_paths/0",
                json!("/usr/lib/tensorplate/speech-runtime/../../lib"),
            )),
            "Invalid(runner_environment)",
            true,
        ),
        (
            "staged root climbing out",
            STT,
            Box::new(set(
                "/staged_path",
                json!("/var/lib/tensorplate/bundles/staging/../../../etc"),
            )),
            "Invalid(staged_path)",
            true,
        ),
        (
            "staged root beyond 4096 characters",
            STT,
            Box::new(set(
                "/staged_path",
                json!(format!("/{}", "s".repeat(4_096))),
            )),
            "Invalid(staged_path)",
            false,
        ),
        (
            "acceptance profile digest not sha256",
            STT,
            Box::new(|doc: &mut Value| {
                doc["configuration"]["acceptance_profile_digest"] = json!("sha256:xyz");
            }),
            "Invalid(configuration.acceptance_profile_digest)",
            false,
        ),
    ];
    for (name, fixture, edit, want, schema_accepts) in cases {
        let mut doc = read_json(fixture);
        edit(&mut doc);
        let text = serde_json::to_string(&doc).expect("text");
        let err = DeploymentDescriptor::from_json(&text).expect_err(name);
        assert_eq!(refusal_of(&err), want, "{name}: {err}");
        assert_eq!(validator().is_valid(&doc), schema_accepts, "{name}: schema");
    }
}

#[test]
fn the_reader_refuses_a_repeated_key_the_value_model_would_hide() {
    let text = read(STT);
    let repeated = text.replacen(
        "\"generation\": 3,",
        "\"generation\": 3,\n  \"generation\": 3,",
        1,
    );
    assert_ne!(repeated, text);
    let err = DeploymentDescriptor::from_json(&repeated).expect_err("repeated key");
    assert_eq!(refusal_of(&err), "Canonical(duplicate_key)", "{err}");
}

#[test]
fn schema_copies_match_their_sources() {
    let schema = read_json("protocol/schemas/deployment_descriptor.json");
    let lines: Vec<&str> = schema["definitions"]["every_budget_line"]["required"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|v| v.as_str().expect("line"))
        .collect();
    assert_eq!(lines, MEMORY_BUDGET_LINE_NAMES);
    let manifest = read_json("protocol/schemas/bundle_manifest.json");
    assert_eq!(
        schema["definitions"]["configuration"]["properties"]["precision_hint"]["enum"],
        manifest["properties"]["precision_hint"]["enum"]
    );
    let modes: Vec<Value> = [AdmissionMode::Production, AdmissionMode::Qualification]
        .iter()
        .map(|m| serde_json::to_value(m).expect("mode"))
        .collect();
    assert_eq!(schema["properties"]["admission_mode"]["enum"], json!(modes));
}

#[test]
fn refusals_map_to_protocol_error_codes() {
    let code_of = |err: DeploymentDescriptorError| ProtocolError::from(err).code;
    for (pointer, value) in [
        ("/schema_version", json!("0.2")),
        ("/canonical_json_version", json!(2)),
    ] {
        let mut doc = read_json(STT);
        *doc.pointer_mut(pointer).expect(pointer) = value;
        let err = DeploymentDescriptor::from_json(&doc.to_string()).expect_err(pointer);
        assert_eq!(code_of(err), ErrorCode::Unsupported, "{pointer}");
    }
    let err = derive_with(STT, &[], |_| {}).expect_err("no runner");
    assert_eq!(code_of(err), ErrorCode::Unsupported);
    let mut profiles = installed_runner_profiles();
    for profile in &mut profiles {
        profile.compute_types = vec![profile.compute_types[0]];
        profile.compute_types[0] = serde_json::from_value(json!("int8")).expect("int8");
    }
    let err = derive_with(STT, &profiles, |_| {}).expect_err("no float16");
    assert_eq!(code_of(err), ErrorCode::Unsupported);
    let mut doc = read_json(STT);
    doc["generation"] = json!(9);
    let err = DeploymentDescriptor::from_json(&doc.to_string()).expect_err("digest");
    assert_eq!(code_of(err), ErrorCode::ConfigInvalid);
}

#[test]
fn descriptor_profile_rules_reject_before_digest_checks() {
    use tensorplate_protocol::BundleRuleCode as Rule;
    for (pointer, value, expected) in [
        (
            "/configuration/precision_hint",
            json!("auto"),
            Rule::ExplicitPrecision,
        ),
        (
            "/configuration/precision_hint",
            json!("fp32"),
            Rule::PrecisionConflict,
        ),
        (
            "/configuration/model_class",
            json!("custom"),
            Rule::ReservedClass,
        ),
    ] {
        let mut value_in = read_json(STT);
        *value_in.pointer_mut(pointer).expect("pointer") = value;
        match DeploymentDescriptor::from_json(&value_in.to_string()).expect_err("profile rule") {
            DeploymentDescriptorError::Profile(error) => {
                assert_eq!(error.rule_code(), Some(expected));
            }
            other => panic!("expected {expected}, got {other}"),
        }
    }
    for (field, expected) in [
        ("speech", Rule::ModelBlock),
        ("runner_profile", Rule::RequiredField),
        ("compute_type", Rule::RequiredField),
        ("pipeline_stages", Rule::RequiredField),
        ("memory_budget_by_domain", Rule::StreamingState),
        ("max_concurrent_sessions", Rule::RequiredField),
    ] {
        let mut value = read_json(STT);
        value["configuration"]
            .as_object_mut()
            .expect("config")
            .remove(field);
        match DeploymentDescriptor::from_json(&value.to_string()).expect_err("missing field") {
            DeploymentDescriptorError::Profile(error) => {
                assert_eq!(error.rule_code(), Some(expected), "{field}");
            }
            other => panic!("expected {expected}, got {other}"),
        }
    }
    let mut batch = read_json(STT);
    batch["configuration"]["speech"]["serving_mode"] = json!("batch");
    batch["configuration"]
        .as_object_mut()
        .expect("config")
        .remove("memory_budget_by_domain");
    match DeploymentDescriptor::from_json(&batch.to_string()).expect_err("batch budget") {
        DeploymentDescriptorError::Profile(error) => {
            assert_eq!(error.rule_code(), Some(Rule::RequiredField));
        }
        other => panic!("{other}"),
    }
    let mut value = read_json(STT);
    for domain in ["guest_ram", "device_vram"] {
        value["configuration"]["memory_budget_by_domain"][domain]["per_session_state_bytes"] =
            json!(0);
    }
    match DeploymentDescriptor::from_json(&value.to_string()).expect_err("zero streaming state") {
        DeploymentDescriptorError::Profile(error) => {
            assert_eq!(error.rule_code(), Some(Rule::StreamingState));
        }
        other => panic!("{other}"),
    }
}

#[test]
fn descriptor_task_field_omissions_retain_the_required_field_code() {
    for (fixture, field) in [
        (STT, "input_audio_formats"),
        (TTS, "voices"),
        (TTS, "output_audio_formats"),
    ] {
        let mut value = read_json(fixture);
        value["configuration"]["speech"]
            .as_object_mut()
            .expect("speech")
            .remove(field);
        match DeploymentDescriptor::from_json(&value.to_string()).expect_err("task field") {
            DeploymentDescriptorError::Profile(error) => assert_eq!(
                error.rule_code(),
                Some(tensorplate_protocol::BundleRuleCode::RequiredField)
            ),
            other => panic!("{other}"),
        }
    }
}
