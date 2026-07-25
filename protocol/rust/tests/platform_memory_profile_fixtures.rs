// SPDX-License-Identifier: Apache-2.0
//
// Canonical platform memory profile records. The committed JSON files under
// `tests/fixtures/` are the language-neutral form of the two canonical
// records; this suite pins them to the in-code constructors, validates them
// against the schema document with a real Draft-07 validator, and keeps the
// Rust vocabulary in lockstep with the schema's enums.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use tensorplate_protocol::{
    PlatformMemoryProfile, PlatformMemoryProfileName, PLATFORM_MEMORY_TELEMETRY_FIELD_NAMES,
};

fn load(name: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
}

fn schema_document() -> serde_json::Value {
    let schema_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../config/schemas/platform_memory_profile.json");
    let raw = std::fs::read_to_string(&schema_path)
        .unwrap_or_else(|e| panic!("read schema {}: {e}", schema_path.display()));
    serde_json::from_str(&raw).expect("schema document parses")
}

#[test]
fn fixtures_match_canonical_constructors() {
    for (name, canonical) in [
        (
            "platform_memory_profile_unified_memory.json",
            PlatformMemoryProfile::unified_memory(),
        ),
        (
            "platform_memory_profile_discrete_gpu.json",
            PlatformMemoryProfile::discrete_gpu(),
        ),
    ] {
        let decoded = PlatformMemoryProfile::from_json(&load(name))
            .unwrap_or_else(|e| panic!("decode failed for {name}: {e}"));
        assert_eq!(
            decoded, canonical,
            "{name} must equal the in-code canonical record"
        );
    }
}

#[test]
fn fixtures_validate_against_schema_document() {
    let validator =
        jsonschema::JSONSchema::compile(&schema_document()).expect("schema compiles as Draft-07");
    for name in [
        "platform_memory_profile_unified_memory.json",
        "platform_memory_profile_discrete_gpu.json",
    ] {
        let instance: serde_json::Value =
            serde_json::from_str(&load(name)).expect("fixture parses");
        assert!(
            validator.is_valid(&instance),
            "{name} must validate against the schema document"
        );
    }
}

fn assert_verdicts_agree(cases: Vec<(&str, serde_json::Value, bool)>) {
    let validator =
        jsonschema::JSONSchema::compile(&schema_document()).expect("schema compiles as Draft-07");
    for (label, instance, expected_valid) in cases {
        assert_eq!(
            validator.is_valid(&instance),
            expected_valid,
            "{label}: Draft-07 verdict diverged from expectation"
        );
        let raw = serde_json::to_string(&instance).expect("serialize");
        assert_eq!(
            PlatformMemoryProfile::from_json(&raw).is_ok(),
            expected_valid,
            "{label}: Rust verdict diverged from expectation"
        );
    }
}

fn unified_value() -> serde_json::Value {
    serde_json::to_value(PlatformMemoryProfile::unified_memory()).expect("serialize")
}

fn discrete_value() -> serde_json::Value {
    serde_json::to_value(PlatformMemoryProfile::discrete_gpu()).expect("serialize")
}

#[test]
fn frozen_semantics_agree_between_schema_and_decoder() {
    // The per-profile domain set and copy-pressure posture are encoded in
    // the schema's if/then conditionals AND in the decoder's semantic
    // validation; every case asserts the SAME expected verdict on both
    // sides, in both the accept and reject quadrants.
    let mut wrong_domains = unified_value();
    wrong_domains["budget_domains"] = discrete_value()["budget_domains"].clone();
    let mut wrong_pressure = discrete_value();
    wrong_pressure["copy_pressure"] = serde_json::json!("not_applicable");
    let mut swapped_order = discrete_value();
    let domains = wrong_domains["budget_domains"].as_array().expect("domains");
    swapped_order["budget_domains"] = serde_json::json!([domains[1], domains[0]]);
    let mut duplicated_domain = discrete_value();
    duplicated_domain["budget_domains"] = serde_json::json!([domains[0], domains[0]]);
    let mut extra_domain = unified_value();
    extra_domain["budget_domains"] =
        serde_json::json!([unified_value()["budget_domains"][0], domains[1]]);

    assert_verdicts_agree(vec![
        ("canonical unified_memory", unified_value(), true),
        ("canonical discrete_gpu", discrete_value(), true),
        ("unified_memory with discrete domains", wrong_domains, false),
        ("discrete_gpu without copy pressure", wrong_pressure, false),
        (
            "discrete_gpu with swapped domain order",
            swapped_order,
            false,
        ),
        (
            "discrete_gpu with duplicated domain",
            duplicated_domain,
            false,
        ),
        ("unified_memory with an extra domain", extra_domain, false),
    ]);
}

#[test]
fn boundary_minimums_agree_between_schema_and_decoder() {
    // The schema's minItems/minLength constraints must be mirrored by the
    // decoder so it is never weaker than the schema document.
    let mut empty_instances = unified_value();
    empty_instances["instances"] = serde_json::json!([]);
    let mut empty_headroom_reporting = unified_value();
    empty_headroom_reporting["headroom_reporting"] = serde_json::json!("");
    let mut empty_gate_note = unified_value();
    empty_gate_note["gate_semantics_note"] = serde_json::json!("");
    let mut empty_measurement_source = unified_value();
    empty_measurement_source["budget_domains"][0]["measurement_source"] = serde_json::json!("");
    let mut empty_headroom_computation = unified_value();
    empty_headroom_computation["budget_domains"][0]["headroom_computation"] = serde_json::json!("");
    let mut empty_instance_name = unified_value();
    empty_instance_name["instances"][0]["instance"] = serde_json::json!("");
    let mut empty_mapping = unified_value();
    empty_mapping["instances"][0]["measurement_source_mapping"] = serde_json::json!("");

    assert_verdicts_agree(vec![
        ("empty instances array", empty_instances, false),
        ("empty headroom_reporting", empty_headroom_reporting, false),
        ("empty gate_semantics_note", empty_gate_note, false),
        (
            "empty domain measurement_source",
            empty_measurement_source,
            false,
        ),
        (
            "empty domain headroom_computation",
            empty_headroom_computation,
            false,
        ),
        ("empty instance identifier", empty_instance_name, false),
        ("empty measurement_source_mapping", empty_mapping, false),
    ]);
}

#[test]
fn rust_vocabulary_matches_schema_document() {
    let schema = schema_document();

    let profile_names: Vec<&str> = schema["properties"]["profile"]["enum"]
        .as_array()
        .expect("profile enum")
        .iter()
        .map(|v| v.as_str().expect("enum entry"))
        .collect();
    assert_eq!(profile_names, ["unified_memory", "discrete_gpu"]);

    let domain_names: Vec<&str> = schema["properties"]["budget_domains"]["items"]["properties"]
        ["domain"]["enum"]
        .as_array()
        .expect("domain enum")
        .iter()
        .map(|v| v.as_str().expect("enum entry"))
        .collect();
    assert_eq!(domain_names, ["shared_pool", "guest_ram", "device_vram"]);

    let telemetry_names: Vec<&str> = schema["definitions"]["telemetry_field_name"]["enum"]
        .as_array()
        .expect("telemetry field enum")
        .iter()
        .map(|v| v.as_str().expect("enum entry"))
        .collect();
    assert_eq!(
        telemetry_names,
        PLATFORM_MEMORY_TELEMETRY_FIELD_NAMES.to_vec(),
        "telemetry field spellings must match the Rust vocabulary"
    );
}

#[test]
fn stub_row_resolves_its_profile_record() {
    // Consumer integration check: a platform row names its profile and
    // resolves the canonical record by that name — the row schema itself
    // lands later, so a stub row stands in for it.
    #[derive(serde::Deserialize)]
    struct StubRow {
        #[allow(dead_code)]
        row_id: String,
        memory_profile: PlatformMemoryProfileName,
    }

    let stub = r#"{"row_id":"stub-jetson-row","memory_profile":"unified_memory"}"#;
    let row: StubRow = serde_json::from_str(stub).expect("stub row parses");
    let record = PlatformMemoryProfile::canonical(row.memory_profile);
    assert_eq!(record, PlatformMemoryProfile::unified_memory());
    assert_eq!(record.budget_domains.len(), 1, "one shared pool");
}
