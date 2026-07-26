// SPDX-License-Identifier: Apache-2.0
//
// The committed platform registry: every v0.2.1 support row and every
// roadmap target. This suite proves the fixtures are what the schemas say
// they are, that the schema document and the Rust decoder always agree,
// and that rows and roadmap targets are not interchangeable.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use tensorplate_platform::{CpuVendor, PlatformSupportRow, RoadmapTarget, SupportLevel};
use tensorplate_protocol::{PlatformMemoryProfile, PlatformMemoryProfileName};

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

fn read_dir_sorted(relative: &str) -> Vec<(String, String)> {
    let dir = repo_path(relative);
    let mut out: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read dir {}: {e}", dir.display()))
        .map(|entry| {
            let path = entry.expect("dir entry").path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .expect("utf-8 file name")
                .to_string();
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            (name, body)
        })
        .filter(|(name, _)| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        })
        .collect();
    out.sort();
    out
}

fn schema(relative: &str) -> serde_json::Value {
    let path = repo_path(relative);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read schema {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("schema document parses")
}

fn row_schema() -> serde_json::Value {
    schema("config/schemas/platform_support_row.json")
}

fn target_schema() -> serde_json::Value {
    schema("config/schemas/roadmap_target.json")
}

fn committed_rows() -> Vec<(String, PlatformSupportRow)> {
    read_dir_sorted("config/platform/rows")
        .into_iter()
        .map(|(name, body)| {
            let row = PlatformSupportRow::from_json(&body)
                .unwrap_or_else(|e| panic!("{name} failed to decode: {e}"));
            (name, row)
        })
        .collect()
}

#[test]
fn every_committed_row_decodes_and_validates() {
    let validator = jsonschema::JSONSchema::compile(&row_schema()).expect("row schema compiles");
    for (name, body) in read_dir_sorted("config/platform/rows") {
        let instance: serde_json::Value =
            serde_json::from_str(&body).unwrap_or_else(|e| panic!("{name} parses: {e}"));
        assert!(
            validator.is_valid(&instance),
            "{name} must validate against the row schema"
        );
        let row =
            PlatformSupportRow::from_json(&body).unwrap_or_else(|e| panic!("{name} decodes: {e}"));
        assert_eq!(
            format!("{}.json", row.row_id()),
            name,
            "{name}: file name must match row_id"
        );
        // Round-trips losslessly through the same validated path.
        let re_emitted = serde_json::to_string(&row).expect("serialize");
        let back = PlatformSupportRow::from_json(&re_emitted).expect("re-decode");
        assert_eq!(row, back, "{name} must round-trip");
    }
}

#[test]
fn the_registry_holds_twelve_rows_at_the_declared_levels() {
    let rows = committed_rows();
    assert_eq!(rows.len(), 12, "twelve exact rows are committed");
    let count = |level: SupportLevel| {
        rows.iter()
            .filter(|(_, r)| r.support_level() == level)
            .count()
    };
    assert_eq!(count(SupportLevel::Production), 5, "five Production rows");
    assert_eq!(count(SupportLevel::Preview), 2, "two Preview rows");
    assert_eq!(count(SupportLevel::Planned), 5, "five Planned rows");
    assert_eq!(
        count(SupportLevel::Experimental),
        0,
        "no Experimental rows in v0.2.1"
    );
}

#[test]
fn row_ids_are_unique_and_match_the_matrix() {
    let rows = committed_rows();
    let ids: BTreeSet<&str> = rows.iter().map(|(_, r)| r.row_id()).collect();
    assert_eq!(ids.len(), rows.len(), "row ids must be unique");

    let expected: BTreeSet<&str> = [
        "jetson-orin-nano-8gb-jp62",
        "ubuntu2404-x86-rtxpro6000se-g4s48",
        "ubuntu2404-x86-l4-g2s8",
        "ubuntu2404-x86-a100-40g-a2hg1",
        "macos26-m1pro-16gb",
        "ubuntu2404-x86-cpu",
        "ubuntu2204-x86-cpu",
        "ubuntu2404-x86-rtxpro6000we-physical",
        "jetson-orin-nx-16gb",
        "jetson-agx-orin-32gb",
        "jetson-agx-orin-64gb",
        "macos26-m4pro-24gb",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        ids, expected,
        "committed rows must match the support matrix"
    );
}

#[test]
fn planned_rows_carry_no_claims() {
    for (name, row) in committed_rows() {
        if row.support_level() != SupportLevel::Planned {
            continue;
        }
        assert!(
            row.evidence().is_none(),
            "{name}: Planned rows have no evidence"
        );
        assert!(
            row.model_class_rows().is_empty(),
            "{name}: Planned rows make no model-class claim"
        );
        assert!(
            !row.is_supported_combination(),
            "{name}: Planned rows are not supported combinations"
        );
    }
}

#[test]
fn production_rows_declare_where_evidence_is_filed() {
    for (name, row) in committed_rows() {
        if row.support_level() != SupportLevel::Production {
            continue;
        }
        let evidence = row
            .evidence()
            .unwrap_or_else(|| panic!("{name}: Production rows declare evidence"));
        assert!(
            evidence.location.contains(row.row_id()),
            "{name}: evidence location should be row-scoped, got `{}`",
            evidence.location
        );
    }
}

#[test]
fn every_accelerator_references_a_real_memory_profile() {
    for (name, row) in committed_rows() {
        let Some(accelerator) = row.accelerator() else {
            continue;
        };
        // The profile record is owned elsewhere; the row only references
        // it by property name, and that reference must resolve.
        let profile = PlatformMemoryProfile::canonical(accelerator.memory_profile);
        assert_eq!(
            profile.profile(),
            accelerator.memory_profile,
            "{name}: memory_profile must resolve to its canonical record"
        );
        match accelerator.memory_profile {
            PlatformMemoryProfileName::UnifiedMemory => {
                assert_eq!(profile.budget_domains().len(), 1, "{name}: one shared pool");
            }
            PlatformMemoryProfileName::DiscreteGpu => {
                assert_eq!(profile.budget_domains().len(), 2, "{name}: two domains");
            }
        }
    }
}

#[test]
fn accelerator_less_rows_record_gpu_utilization_as_absent() {
    for (name, row) in committed_rows() {
        if row.accelerator().is_some() {
            continue;
        }
        let gate = &row.gate_semantics().gpu_utilization;
        assert_eq!(
            gate.gate,
            tensorplate_platform::GateValue::NotApplicable,
            "{name}: no accelerator means no GPU utilization"
        );
        assert!(gate.reason.is_some(), "{name}: absence needs a reason");
    }
}

#[test]
fn every_not_applicable_gate_states_why() {
    for (name, row) in committed_rows() {
        for (signal, gate) in row.gate_semantics().signals() {
            if gate.gate == tensorplate_platform::GateValue::NotApplicable {
                assert!(
                    gate.reason.as_deref().is_some_and(|r| !r.is_empty()),
                    "{name}: {signal} is not_applicable without a reason"
                );
            }
        }
    }
}

#[test]
fn every_committed_roadmap_target_decodes_and_validates() {
    let validator =
        jsonschema::JSONSchema::compile(&target_schema()).expect("target schema compiles");
    let targets = read_dir_sorted("config/platform/roadmap_targets");
    assert_eq!(targets.len(), 4, "four roadmap targets are committed");
    let mut ids = BTreeSet::new();
    for (name, body) in targets {
        let instance: serde_json::Value =
            serde_json::from_str(&body).unwrap_or_else(|e| panic!("{name} parses: {e}"));
        assert!(
            validator.is_valid(&instance),
            "{name} must validate against the roadmap-target schema"
        );
        let target =
            RoadmapTarget::from_json(&body).unwrap_or_else(|e| panic!("{name} decodes: {e}"));
        assert_eq!(format!("{}.json", target.target_id()), name);
        ids.insert(target.target_id().to_string());
    }
    let expected: BTreeSet<String> = [
        "blackwell-dc-single-gpu",
        "pkg-macos-notarized",
        "rocm-mi300x",
        "rocm-mi400",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(ids, expected);
}

#[test]
fn rows_and_roadmap_targets_are_not_interchangeable() {
    let row_validator = jsonschema::JSONSchema::compile(&row_schema()).expect("row schema");
    let target_validator =
        jsonschema::JSONSchema::compile(&target_schema()).expect("target schema");

    for (name, body) in read_dir_sorted("config/platform/roadmap_targets") {
        let instance: serde_json::Value = serde_json::from_str(&body).expect("parses");
        assert!(
            !row_validator.is_valid(&instance),
            "{name} must fail the row schema: a target is never a row"
        );
        assert!(
            PlatformSupportRow::from_json(&body).is_err(),
            "{name} must not decode as a row"
        );
    }
    for (name, body) in read_dir_sorted("config/platform/rows") {
        let instance: serde_json::Value = serde_json::from_str(&body).expect("parses");
        assert!(
            !target_validator.is_valid(&instance),
            "{name} must fail the roadmap-target schema"
        );
        assert!(
            RoadmapTarget::from_json(&body).is_err(),
            "{name} must not decode as a roadmap target"
        );
    }
}

#[test]
fn roadmap_target_ids_are_disjoint_from_row_ids() {
    let row_ids: BTreeSet<String> = committed_rows()
        .iter()
        .map(|(_, r)| r.row_id().to_string())
        .collect();
    for (_, body) in read_dir_sorted("config/platform/roadmap_targets") {
        let target = RoadmapTarget::from_json(&body).expect("decodes");
        assert!(
            !row_ids.contains(target.target_id()),
            "`{}` collides with a row id",
            target.target_id()
        );
    }
}

// --- schema/decoder verdict agreement -----------------------------------

fn a_valid_row_value() -> serde_json::Value {
    let body = read_dir_sorted("config/platform/rows")
        .into_iter()
        .find(|(name, _)| name == "ubuntu2404-x86-l4-g2s8.json")
        .expect("the L4 row is committed")
        .1;
    serde_json::from_str(&body).expect("parses")
}

fn a_valid_cpu_row_value() -> serde_json::Value {
    let body = read_dir_sorted("config/platform/rows")
        .into_iter()
        .find(|(name, _)| name == "ubuntu2404-x86-cpu.json")
        .expect("the CPU row is committed")
        .1;
    serde_json::from_str(&body).expect("parses")
}

fn assert_row_verdicts_agree(cases: Vec<(&str, serde_json::Value, bool)>) {
    let validator = jsonschema::JSONSchema::compile(&row_schema()).expect("row schema compiles");
    for (label, instance, expected_valid) in cases {
        assert_eq!(
            validator.is_valid(&instance),
            expected_valid,
            "{label}: Draft-07 verdict diverged from expectation"
        );
        let raw = serde_json::to_string(&instance).expect("serialize");
        assert_eq!(
            PlatformSupportRow::from_json(&raw).is_ok(),
            expected_valid,
            "{label}: from_json verdict diverged from expectation"
        );
    }
}

#[test]
fn structural_violations_agree_between_schema_and_decoder() {
    let mut unknown_field = a_valid_row_value();
    unknown_field["vendor_extension"] = serde_json::json!("nvidia");
    let mut missing_required = a_valid_row_value();
    missing_required
        .as_object_mut()
        .expect("object")
        .remove("gate_semantics");
    let mut unknown_nested_field = a_valid_row_value();
    unknown_nested_field["os"]["codename"] = serde_json::json!("noble");
    let mut bad_row_id = a_valid_row_value();
    bad_row_id["row_id"] = serde_json::json!("Ubuntu2404-L4");
    let mut bad_component_id = a_valid_row_value();
    bad_component_id["kernel_driver_stack"]["components"] =
        serde_json::json!([{"component": "NVIDIA-Driver", "version": "550"}]);
    let mut empty_backends = a_valid_row_value();
    empty_backends["backend_packages"] = serde_json::json!([]);
    let mut wrong_version = a_valid_row_value();
    wrong_version["schema_version"] = serde_json::json!("9.9");

    assert_row_verdicts_agree(vec![
        ("canonical row", a_valid_row_value(), true),
        ("unknown top-level field", unknown_field, false),
        ("missing required field", missing_required, false),
        ("unknown nested field", unknown_nested_field, false),
        ("non-canonical row_id", bad_row_id, false),
        ("non-canonical component id", bad_component_id, false),
        ("empty backend_packages", empty_backends, false),
        ("unsupported schema_version", wrong_version, false),
    ]);
}

#[test]
fn shape_and_enum_forms_agree_between_schema_and_decoder() {
    let mut map_form_enum = a_valid_row_value();
    map_form_enum["cpu"]["architecture"] = serde_json::json!({"x86_64": null});
    let mut map_form_support_level = a_valid_row_value();
    map_form_support_level["support_level"] = serde_json::json!({"Production": null});
    let mut seq_form_os = a_valid_row_value();
    seq_form_os["os"] = serde_json::json!(["Ubuntu", "24.04"]);
    let mut seq_form_backend = a_valid_row_value();
    seq_form_backend["backend_packages"] = serde_json::json!([[
        "python_pytorch",
        "apt",
        ["tensorplate-backend-python-pytorch"]
    ]]);
    let seq_form_row = serde_json::json!(["0.1", "ubuntu2404-x86-l4-g2s8"]);
    let mut unknown_enum_value = a_valid_row_value();
    unknown_enum_value["cpu"]["vendors"] = serde_json::json!(["via"]);

    assert_row_verdicts_agree(vec![
        ("map-form architecture", map_form_enum, false),
        ("map-form support_level", map_form_support_level, false),
        ("sequence-form os", seq_form_os, false),
        ("sequence-form backend package set", seq_form_backend, false),
        ("sequence-form whole row", seq_form_row, false),
        ("unknown cpu vendor value", unknown_enum_value, false),
    ]);
}

#[test]
fn support_level_invariants_agree_between_schema_and_decoder() {
    let mut production_without_evidence = a_valid_row_value();
    production_without_evidence
        .as_object_mut()
        .expect("object")
        .remove("evidence");
    let mut planned_with_evidence = a_valid_row_value();
    planned_with_evidence["support_level"] = serde_json::json!("Planned");
    let mut planned_with_model_rows = a_valid_row_value();
    planned_with_model_rows["support_level"] = serde_json::json!("Planned");
    planned_with_model_rows
        .as_object_mut()
        .expect("object")
        .remove("evidence");
    planned_with_model_rows["model_class_rows"] =
        serde_json::json!([{"model_class_row": "chunked_policy", "support_level": "Production"}]);
    let mut planned_recorded = a_valid_row_value();
    planned_recorded["support_level"] = serde_json::json!("Planned");
    planned_recorded
        .as_object_mut()
        .expect("object")
        .remove("evidence");
    planned_recorded["provenance"] = serde_json::json!("recorded");
    let mut valid_planned = a_valid_row_value();
    valid_planned["support_level"] = serde_json::json!("Planned");
    valid_planned
        .as_object_mut()
        .expect("object")
        .remove("evidence");
    // A Planned row makes no model-class claim either.
    valid_planned["model_class_rows"] = serde_json::json!([]);

    assert_row_verdicts_agree(vec![
        (
            "Production without evidence",
            production_without_evidence,
            false,
        ),
        ("Planned with evidence", planned_with_evidence, false),
        ("Planned with model rows", planned_with_model_rows, false),
        ("Planned marked recorded", planned_recorded, false),
        ("Planned without claims", valid_planned, true),
    ]);
}

#[test]
fn gate_invariants_agree_between_schema_and_decoder() {
    let mut not_applicable_without_reason = a_valid_row_value();
    not_applicable_without_reason["gate_semantics"]["power"] =
        serde_json::json!({"gate": "not_applicable"});
    let mut empty_reason = a_valid_row_value();
    empty_reason["gate_semantics"]["power"] =
        serde_json::json!({"gate": "not_applicable", "reason": ""});
    let mut unknown_gate_value = a_valid_row_value();
    unknown_gate_value["gate_semantics"]["memory"] = serde_json::json!({"gate": "advisory"});
    let mut missing_signal = a_valid_row_value();
    missing_signal["gate_semantics"]
        .as_object_mut()
        .expect("object")
        .remove("throttle");
    let mut with_reason = a_valid_row_value();
    with_reason["gate_semantics"]["power"] =
        serde_json::json!({"gate": "not_applicable", "reason": "not exposed on this row"});

    assert_row_verdicts_agree(vec![
        (
            "not_applicable without reason",
            not_applicable_without_reason,
            false,
        ),
        ("empty gate reason", empty_reason, false),
        ("unknown gate value", unknown_gate_value, false),
        ("missing signal", missing_signal, false),
        ("not_applicable with reason", with_reason, true),
    ]);
}

#[test]
fn accelerator_invariants_agree_between_schema_and_decoder() {
    let mut cpu_row_claiming_gpu = a_valid_cpu_row_value();
    cpu_row_claiming_gpu["gate_semantics"]["gpu_utilization"] =
        serde_json::json!({"gate": "context_only"});
    let mut multi_vendor_with_accelerator = a_valid_row_value();
    multi_vendor_with_accelerator["cpu"]["vendors"] = serde_json::json!(["intel", "amd"]);
    let mut unknown_memory_profile = a_valid_row_value();
    unknown_memory_profile["accelerator"]["memory_profile"] = serde_json::json!("hbm");

    assert_row_verdicts_agree(vec![
        (
            "canonical accelerator-less row",
            a_valid_cpu_row_value(),
            true,
        ),
        (
            "accelerator-less row reporting GPU utilization",
            cpu_row_claiming_gpu,
            false,
        ),
        (
            "multiple vendors with an accelerator",
            multi_vendor_with_accelerator,
            false,
        ),
        ("unknown memory profile", unknown_memory_profile, false),
    ]);
}

#[test]
fn accelerator_memory_bytes_stay_exact() {
    let mut zero = a_valid_row_value();
    zero["accelerator"]["memory_bytes"] = serde_json::json!(0);
    let mut missing = a_valid_row_value();
    missing["accelerator"]
        .as_object_mut()
        .expect("accelerator object")
        .remove("memory_bytes");
    let mut oversized = a_valid_row_value();
    oversized["accelerator"]["memory_bytes"] = serde_json::json!(9_007_199_254_740_992_u64);
    let mut negative = a_valid_row_value();
    negative["accelerator"]["memory_bytes"] = serde_json::json!(-1);

    assert_row_verdicts_agree(vec![
        ("memory_bytes above the safe range", oversized, false),
        ("negative memory_bytes", negative, false),
        ("zero memory_bytes", zero, false),
        ("missing memory_bytes", missing, false),
    ]);

    // Float spellings of exact integers must decode to the declared value,
    // not a neighbouring one, and fractional tokens must fail closed.
    let mut float_spelling = a_valid_row_value();
    float_spelling["accelerator"]["memory_bytes"] = serde_json::json!(25_769_803_776_u64);
    let raw = serde_json::to_string(&float_spelling)
        .expect("serialize")
        .replace("25769803776", "25769803776.0");
    let decoded = PlatformSupportRow::from_json(&raw).expect("integral float decodes");
    assert_eq!(
        decoded.accelerator().expect("accelerator").memory_bytes,
        25_769_803_776
    );

    let fractional = raw.replace("25769803776.0", "25769803776.5");
    PlatformSupportRow::from_json(&fractional).expect_err("fractional must reject");
}

#[test]
fn duplicate_json_keys_reject() {
    let body = read_dir_sorted("config/platform/rows")
        .into_iter()
        .find(|(name, _)| name == "macos26-m1pro-16gb.json")
        .expect("the macOS row is committed")
        .1;
    let canonical =
        serde_json::to_string(&serde_json::from_str::<serde_json::Value>(&body).expect("parses"))
            .expect("serialize");
    let duplicated = canonical.replacen(
        r#""support_level":"Production""#,
        r#""support_level":"Planned","support_level":"Production""#,
        1,
    );
    assert_ne!(duplicated, canonical, "the duplicate key was injected");
    assert!(
        PlatformSupportRow::from_json(&duplicated).is_err(),
        "from_json must reject duplicate keys"
    );
}

#[test]
fn roadmap_targets_cannot_declare_row_fields() {
    let base = read_dir_sorted("config/platform/roadmap_targets")
        .into_iter()
        .next()
        .expect("a target is committed")
        .1;
    let validator =
        jsonschema::JSONSchema::compile(&target_schema()).expect("target schema compiles");
    for forbidden in [
        "row_id",
        "support_level",
        "model_class_rows",
        "gate_semantics",
        "evidence",
    ] {
        let mut instance: serde_json::Value = serde_json::from_str(&base).expect("parses");
        instance[forbidden] = serde_json::json!("anything");
        assert!(
            !validator.is_valid(&instance),
            "a roadmap target must not accept `{forbidden}`"
        );
        let raw = serde_json::to_string(&instance).expect("serialize");
        assert!(
            RoadmapTarget::from_json(&raw).is_err(),
            "the decoder must reject `{forbidden}` too"
        );
    }
}

#[test]
fn explicit_null_is_not_absence() {
    // `#[serde(default)] Option<T>` treats a present `null` as absent,
    // but no schema `type` admits null: an optional property is either
    // absent or a value of its declared type.
    let mut null_evidence = a_valid_row_value();
    null_evidence["evidence"] = serde_json::Value::Null;
    let mut null_image_identity = a_valid_row_value();
    null_image_identity["os"]["image_identity"] = serde_json::Value::Null;
    let mut null_memory = a_valid_row_value();
    null_memory["accelerator"]["memory_bytes"] = serde_json::Value::Null;
    let mut null_reason = a_valid_row_value();
    null_reason["gate_semantics"]["thermal"]["reason"] = serde_json::Value::Null;
    let mut null_accelerator = a_valid_row_value();
    null_accelerator["accelerator"] = serde_json::Value::Null;

    assert_row_verdicts_agree(vec![
        ("null evidence", null_evidence, false),
        ("null image_identity", null_image_identity, false),
        ("null memory_bytes", null_memory, false),
        ("null gate reason", null_reason, false),
        ("null accelerator", null_accelerator, false),
    ]);
}

#[test]
fn blank_strings_are_decoder_stricter_than_the_schema() {
    // Whitespace-only satisfies `minLength: 1`, and a `\S` pattern cannot
    // close that portably: regex dialects disagree about which code points
    // are whitespace (Rust's `str::trim` excludes U+FEFF while ECMA-262
    // `\s` includes it, and the reverse holds for U+3000). The rule is
    // therefore decoder-enforced, which is a documented one-directional
    // divergence: the decoder is stricter, never weaker.
    let validator = jsonschema::JSONSchema::compile(&row_schema()).expect("row schema compiles");
    let mut blank_sku = a_valid_row_value();
    blank_sku["accelerator"]["sku"] = serde_json::json!("   ");
    let mut blank_reason = a_valid_cpu_row_value();
    blank_reason["gate_semantics"]["power"] =
        serde_json::json!({"gate": "not_applicable", "reason": " "});
    let mut blank_evidence = a_valid_row_value();
    blank_evidence["evidence"]["location"] = serde_json::json!("\t");

    for (label, instance) in [
        ("whitespace-only sku", blank_sku),
        ("whitespace-only gate reason", blank_reason),
        ("whitespace-only evidence location", blank_evidence),
    ] {
        assert!(
            validator.is_valid(&instance),
            "{label}: the schema cannot express this rule"
        );
        let raw = serde_json::to_string(&instance).expect("serialize");
        assert!(
            PlatformSupportRow::from_json(&raw).is_err(),
            "{label}: the decoder must still reject it"
        );
    }
}

#[test]
fn illegal_json_number_spellings_are_not_repaired() {
    // Leading zeros are not legal JSON. Canonicalizing them would let
    // `from_json` accept text a JSON parser — and therefore the schema
    // validator — cannot read at all.
    let body = std::fs::read_to_string(repo_path(
        "config/platform/rows/ubuntu2404-x86-l4-g2s8.json",
    ))
    .expect("read the L4 row");
    let illegal = body.replace("25769803776", "025769803776");
    assert!(
        serde_json::from_str::<serde_json::Value>(&illegal).is_err(),
        "the mutated document is not JSON"
    );
    assert!(
        PlatformSupportRow::from_json(&illegal).is_err(),
        "from_json must not repair illegal number spellings"
    );
}

#[test]
fn byte_values_decode_exactly_through_the_only_path() {
    // `PlatformSupportRow` has no `Deserialize` impl, so there is one
    // decoding path and no way for two paths to disagree on a value the
    // JSON parser would round.
    let body = std::fs::read_to_string(repo_path(
        "config/platform/rows/ubuntu2404-x86-l4-g2s8.json",
    ))
    .expect("read the L4 row");
    let float_spelled = body.replace("25769803776", "25769803776.0");
    let row = PlatformSupportRow::from_json(&float_spelled).expect("integral float decodes");
    assert_eq!(
        row.accelerator().expect("accelerator").memory_bytes,
        25_769_803_776
    );
    for bad in [
        "25769803776.5",
        "1.0000000000000001",
        "1e-9223372036854775809",
    ] {
        let mutated = body.replace("25769803776", bad);
        assert!(
            PlatformSupportRow::from_json(&mutated).is_err(),
            "`{bad}` must fail closed"
        );
    }
}

#[test]
fn enum_spellings_are_welded_to_their_serialized_form() {
    // Serialization goes through `as_str`, so a spelling that is written
    // is by construction one that decodes.
    use tensorplate_platform::{CpuArchitecture, CpuVendor, GateValue, Partitioning, SupportLevel};
    macro_rules! assert_welded {
        ($ty:ty, [$($variant:expr),+ $(,)?]) => {
            $(
                let json = serde_json::to_string(&$variant).expect("serialize");
                assert_eq!(json, format!("\"{}\"", $variant.as_str()));
                let back: $ty = serde_json::from_str(&json).expect("re-deserialize");
                assert_eq!(back, $variant);
            )+
        };
    }
    assert_welded!(
        CpuArchitecture,
        [CpuArchitecture::X86_64, CpuArchitecture::Arm64]
    );
    assert_welded!(
        CpuVendor,
        [
            CpuVendor::Amd,
            CpuVendor::Intel,
            CpuVendor::Apple,
            CpuVendor::NvidiaSoc
        ]
    );
    assert_welded!(
        Partitioning,
        [Partitioning::Unsupported, Partitioning::NotApplicable]
    );
    assert_welded!(
        GateValue,
        [
            GateValue::LoadBearing,
            GateValue::ContextOnly,
            GateValue::NotApplicable
        ]
    );
    assert_welded!(
        SupportLevel,
        [
            SupportLevel::Production,
            SupportLevel::Preview,
            SupportLevel::Experimental,
            SupportLevel::Planned
        ]
    );
}

#[test]
fn only_production_and_preview_are_supported_combinations() {
    // Experimental rows are listed separately in release notes and must
    // not count as supported, exactly like Planned rows.
    for (name, row) in committed_rows() {
        let expected = matches!(
            row.support_level(),
            SupportLevel::Production | SupportLevel::Preview
        );
        assert_eq!(
            row.is_supported_combination(),
            expected,
            "{name}: supported-combination status must follow the support level"
        );
    }

    // No Experimental row is committed today, so exercise the rule on a
    // synthetic one rather than letting it go untested.
    let mut experimental = a_valid_row_value();
    experimental["support_level"] = serde_json::json!("Experimental");
    let raw = serde_json::to_string(&experimental).expect("serialize");
    let synthetic = PlatformSupportRow::from_json(&raw).expect("Experimental rows are valid");
    assert!(
        !synthetic.is_supported_combination(),
        "an Experimental row is not a supported combination"
    );
}

#[test]
fn cpu_vendor_sets_are_exact() {
    // The accelerator-less utility rows carry the documented AMD/Intel
    // posture rather than a wildcard, so vendor support is decided by
    // registry membership alone.
    for (name, row) in committed_rows() {
        let vendors = &row.cpu().vendors;
        assert!(!vendors.is_empty(), "{name}: a row names its vendors");
        if row.accelerator().is_some() {
            assert_eq!(
                vendors.len(),
                1,
                "{name}: an exact accelerator SKU pins one host vendor"
            );
        }
        if row.row_id().ends_with("-x86-cpu") {
            assert_eq!(
                vendors,
                &[CpuVendor::Amd, CpuVendor::Intel],
                "{name}: the CPU-only rows cover AMD and Intel"
            );
            assert!(!row.cpu().covers_vendor(CpuVendor::Apple));
        }
    }
}

#[test]
fn deploy_smoke_rows_declare_preview_model_pointers() {
    // doctor renders model-class posture from these pointers, so the
    // deploy-smoke rows must carry one rather than relying on a hardcoded
    // list downstream.
    for (name, row) in committed_rows() {
        let expected_preview = matches!(
            row.row_id(),
            "ubuntu2404-x86-l4-g2s8" | "ubuntu2404-x86-a100-40g-a2hg1" | "macos26-m1pro-16gb"
        );
        if !expected_preview {
            continue;
        }
        let pointers = row.model_class_rows();
        assert!(
            !pointers.is_empty(),
            "{name}: deploy-smoke rows declare their Preview model posture"
        );
        assert!(
            pointers
                .iter()
                .all(|p| p.support_level == SupportLevel::Preview),
            "{name}: model rows stay Preview until a model pathway adds evidence"
        );
    }
}
