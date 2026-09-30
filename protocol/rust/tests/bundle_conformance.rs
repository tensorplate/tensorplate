// SPDX-License-Identifier: Apache-2.0
//
// bundle format: bundle conformance suite.
//
// These tests assert the v0.1.0 bundle contract end-to-end against the
// checked-in fixtures under `test/models/bundles/v0_1/`. They run on
// the host without TensorRT, CUDA, PyTorch, or Vitis AI SDKs — the
// parser/verifier path is deliberately SDK-free in v0.1.0.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tensorplate_protocol::bundle::{
    evaluate_compatibility, parse_bundle, BackendCapabilityView, BackendProfile, DeviceContext,
    ParseError,
};
use tensorplate_protocol::bundle_manifest::{ArtifactRole, DeviceFamily, RECOGNIZED_BACKEND_HINTS};
use tensorplate_protocol::model_spec::ModelClass;

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("test/models/bundles/v0_1")
}

fn jetson_device(backends: &[(&str, BackendCapabilityView, &[&str])]) -> DeviceContext {
    DeviceContext {
        runtime_version: Some("0.1.0".into()),
        device_family: Some(DeviceFamily::JetsonOrin),
        device_memory_bytes: Some(16 * 1024 * 1024 * 1024),
        backends: backends
            .iter()
            .map(|(name, caps, kinds)| BackendProfile {
                backend: (*name).to_string(),
                capabilities: *caps,
                supported_precision: vec![
                    "auto".into(),
                    "fp32".into(),
                    "fp16".into(),
                    "int8".into(),
                ],
                supported_artifact_kinds: kinds.iter().map(|k| (*k).to_string()).collect(),
            })
            .collect(),
    }
}

// ----- Valid fixtures -----------------------------------------------------

#[test]
fn vision_tensorrt_parses_and_passes_compatibility_on_jetson() {
    let root = fixtures_root().join("vision_tensorrt");
    let d = parse_bundle(&root).expect("vision fixture must parse");
    assert_eq!(d.manifest.name, "yolov8n-vision");
    assert_eq!(d.manifest.model_class, ModelClass::Vision);
    assert_eq!(d.manifest.backend_hint, "tensorrt");
    assert_eq!(d.manifest.inputs.len(), 1);
    assert_eq!(d.manifest.outputs.len(), 1);
    assert!(d.manifest.model_blocks.vision.is_some());
    let device = jetson_device(&[(
        "tensorrt",
        BackendCapabilityView {
            fixed_shape: true,
            deterministic_latency: true,
            ..BackendCapabilityView::default()
        },
        &["tensorrt_engine"],
    )]);
    let r = evaluate_compatibility(&d, &device);
    assert!(r.ok, "expected ok, got {r:?}");
}

#[test]
fn smolvla_python_pytorch_uses_named_multi_input_and_action_output() {
    let root = fixtures_root().join("smolvla_python_pytorch");
    let d = parse_bundle(&root).expect("smolvla fixture must parse");
    assert_eq!(d.manifest.model_class, ModelClass::Vla);
    assert_eq!(d.manifest.backend_hint, "python_pytorch");
    assert!(d.manifest.inputs.len() >= 2);
    assert_eq!(d.manifest.outputs.len(), 1);
    assert!(d.manifest.outputs[0].control_loop);
    let vla = d
        .manifest
        .model_blocks
        .vla
        .as_ref()
        .expect("vla block present");
    assert!(vla.control_frequency_hz.is_some());
    let device = jetson_device(&[(
        "python_pytorch",
        BackendCapabilityView {
            async_: true,
            deterministic_latency: true,
            control_loop_integration: true,
            ..BackendCapabilityView::default()
        },
        &["python_pytorch_entry"],
    )]);
    let r = evaluate_compatibility(&d, &device);
    assert!(
        r.ok,
        "smolvla compat must pass on jetson when sidecar published, got {r:?}"
    );
}

#[test]
fn mps_python_pytorch_smoke_bundle_has_verified_config_artifact() {
    let root = fixtures_root().join("mps_python_pytorch_smoke");
    let d = parse_bundle(&root).expect("MPS smoke fixture must parse");
    assert_eq!(d.manifest.model_class, ModelClass::Custom);
    assert_eq!(d.manifest.backend_hint, "python_pytorch");
    let model_artifact = d
        .artifacts
        .iter()
        .find(|artifact| artifact.role == ArtifactRole::Model)
        .expect("model artifact present");
    assert!(model_artifact.relative_path.ends_with("mps-smoke.json"));
}

#[test]
fn x86_fixture_smoke_bundle_targets_the_cloud_row_device_family() {
    // The deploy-smoke input for the Ubuntu x86_64 cloud rows. It names
    // `x86_64` rather than `any` so admission matches it against that
    // row's agent config exactly, instead of passing through the
    // either-side-Any escape and proving nothing about the match.
    let root = fixtures_root().join("x86_fixture_smoke");
    let d = parse_bundle(&root).expect("x86 smoke fixture must parse");
    assert_eq!(d.manifest.model_class, ModelClass::Custom);
    assert_eq!(d.manifest.backend_hint, "python_pytorch");
    assert_eq!(
        d.manifest.target_hardware.device_family,
        DeviceFamily::X86_64,
        "the bundle must name the cloud rows' device family"
    );
    let model_artifact = d
        .artifacts
        .iter()
        .find(|artifact| artifact.role == ArtifactRole::Model)
        .expect("model artifact present");
    assert!(model_artifact.relative_path.ends_with("x86-smoke.json"));
}

#[test]
fn x86_cuda_smoke_bundle_selects_the_cuda_fixture_profile() {
    // The accelerator-side companion to the fixture smoke bundle: same
    // row and device family, but its config selects a profile that only
    // loads after a CUDA kernel has run.
    let root = fixtures_root().join("x86_cuda_smoke");
    let d = parse_bundle(&root).expect("x86 CUDA smoke fixture must parse");
    assert_eq!(d.manifest.model_class, ModelClass::Custom);
    assert_eq!(d.manifest.backend_hint, "python_pytorch");
    assert_eq!(
        d.manifest.target_hardware.device_family,
        DeviceFamily::X86_64,
        "the bundle must name the cloud rows' device family"
    );
    let model_artifact = d
        .artifacts
        .iter()
        .find(|artifact| artifact.role == ArtifactRole::Model)
        .expect("model artifact present");
    assert!(model_artifact
        .relative_path
        .ends_with("x86-cuda-smoke.json"));
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("x86-cuda-smoke.json")).unwrap())
            .expect("config parses");
    assert_eq!(config["backend_profile"], "cuda_fixture");
    assert_eq!(config["device"], "cuda");
}

fn x86_64_python_pytorch_device(runtime_version: &str) -> DeviceContext {
    DeviceContext {
        runtime_version: Some(runtime_version.into()),
        device_family: Some(DeviceFamily::X86_64),
        device_memory_bytes: None,
        backends: vec![BackendProfile {
            backend: "python_pytorch".into(),
            capabilities: BackendCapabilityView::default(),
            supported_precision: vec!["auto".into(), "fp32".into(), "fp16".into()],
            supported_artifact_kinds: vec!["python_pytorch_entry".into()],
        }],
    }
}

/// A runner profile verifies the files its entry lists, and the agent the
/// files the manifest lists, so the two lists must name the same files with
/// the same digests: a file only one of them names escapes one of the checks.
fn assert_candidate_speech_bundle(root: &Path, profile: &str) {
    let d = parse_bundle(root).expect("candidate fixture must parse");
    assert_eq!(d.manifest.model_class, ModelClass::Custom);
    assert_eq!(d.manifest.backend_hint, "python_pytorch");
    assert_eq!(
        d.manifest.target_hardware.device_family,
        DeviceFamily::X86_64
    );

    let entry_path = d.model_artifact_path().expect("model artifact present");
    let entry: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(entry_path).unwrap()).expect("entry parses");
    assert_eq!(entry["backend_profile"], profile);
    let listed = entry["artifact_set"]
        .as_array()
        .expect("artifact_set array");
    let from_entry: BTreeMap<&str, &str> = listed
        .iter()
        .map(|item| {
            (
                item["path"].as_str().expect("path"),
                item["digest"].as_str().expect("digest"),
            )
        })
        .collect();
    assert_eq!(
        from_entry.len(),
        listed.len(),
        "artifact_set repeats a path"
    );
    let from_manifest: BTreeMap<&str, &str> = d
        .manifest
        .artifacts
        .iter()
        .filter(|a| a.role != ArtifactRole::Model)
        .map(|a| (a.path.as_str(), a.digest.as_str()))
        .collect();
    assert_eq!(from_entry, from_manifest);

    // The runner profiles ship with the 0.3 line, so an older runtime must
    // refuse the bundle before it reaches a sidecar that cannot load it.
    let current = evaluate_compatibility(&d, &x86_64_python_pytorch_device("0.3.1"));
    assert!(current.ok, "got {current:?}");
    let older = evaluate_compatibility(&d, &x86_64_python_pytorch_device("0.2.1"));
    assert!(
        older
            .violations
            .iter()
            .any(|v| v.code() == "unsupported_runtime"),
        "got {older:?}"
    );
}

#[test]
fn stt_whisper_candidate_bundle_lists_its_artifact_set_in_the_entry() {
    assert_candidate_speech_bundle(
        &fixtures_root().join("stt_whisper_candidate"),
        "faster_whisper",
    );
}

#[test]
fn tts_kokoro_candidate_bundle_lists_its_artifact_set_in_the_entry() {
    assert_candidate_speech_bundle(&fixtures_root().join("tts_kokoro_candidate"), "kokoro");
}

#[test]
fn language_reserved_parses_without_requiring_runtime() {
    let root = fixtures_root().join("language_reserved");
    let d = parse_bundle(&root).expect("language fixture must parse");
    assert_eq!(d.manifest.model_class, ModelClass::Language);
    let language = d
        .manifest
        .model_blocks
        .language
        .as_ref()
        .expect("language block present");
    let tokenizer = language.tokenizer.as_ref().expect("tokenizer present");
    assert_eq!(tokenizer.reference, "tokenizer.model");
    // generation_config exists with default/empty values.
    let gen = language
        .generation_config
        .as_ref()
        .expect("generation_config reserved");
    assert!(!gen.streaming);
}

#[test]
fn vitis_synthetic_parses_but_compat_rejects_when_backend_unavailable() {
    let root = fixtures_root().join("vitis_synthetic");
    let d = parse_bundle(&root).expect("vitis fixture must parse");
    assert_eq!(d.manifest.backend_hint, "vitis_ai");
    let model_art = d
        .artifacts
        .iter()
        .find(|a| a.role == ArtifactRole::Model)
        .expect("model artifact present");
    assert!(model_art.relative_path.ends_with(".xmodel"));
    assert!(d.manifest.precision.vitis_ai.dpu_arch.is_some());

    // Jetson device — vitis_ai is NOT in available_backends.
    let device = DeviceContext {
        runtime_version: Some("0.1.0".into()),
        device_family: Some(DeviceFamily::JetsonOrin),
        device_memory_bytes: Some(16 * 1024 * 1024 * 1024),
        backends: vec![BackendProfile {
            backend: "tensorrt".into(),
            capabilities: BackendCapabilityView::default(),
            supported_precision: vec!["fp16".into()],
            supported_artifact_kinds: vec!["tensorrt_engine".into()],
        }],
    };
    let r = evaluate_compatibility(&d, &device);
    assert!(!r.ok);
    assert!(r
        .violations
        .iter()
        .any(|v| v.code() == "unavailable_backend"));
}

// ----- Invalid fixtures ----------------------------------------------------

#[test]
fn invalid_corrupt_artifact_raises_digest_mismatch() {
    let root = fixtures_root().join("invalid_corrupt_artifact");
    let err = parse_bundle(&root).expect_err("must reject");
    assert!(
        matches!(err, ParseError::ArtifactDigestMismatch { .. }),
        "got {err:?}"
    );
}

#[test]
fn invalid_unsafe_path_raises_typed_error() {
    let root = fixtures_root().join("invalid_unsafe_path");
    let err = parse_bundle(&root).expect_err("must reject");
    // Path safety is enforced by both BundleManifest::validate and the parser
    // layer; accept either typed shape.
    match err {
        ParseError::ManifestSemantics(_) | ParseError::UnsafeArtifactPath { .. } => {}
        other => panic!("unexpected error variant: {other:?}"),
    }
}

#[test]
fn invalid_missing_artifact_raises_artifact_missing() {
    let root = fixtures_root().join("invalid_missing_artifact");
    let err = parse_bundle(&root).expect_err("must reject");
    assert!(
        matches!(err, ParseError::ArtifactMissing { .. }),
        "got {err:?}"
    );
}

#[test]
fn invalid_duplicate_io_raises_duplicate_input_name() {
    let root = fixtures_root().join("invalid_duplicate_io");
    let err = parse_bundle(&root).expect_err("must reject");
    match err {
        ParseError::ManifestSemantics(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("duplicate name") || msg.contains("DuplicateInputName"),
                "got: {msg}"
            );
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn invalid_language_block_on_vision_class_is_rejected() {
    let root = fixtures_root().join("invalid_language_block_class");
    let err = parse_bundle(&root).expect_err("must reject");
    match err {
        ParseError::ManifestSemantics(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("language") || msg.contains("model_class"),
                "got: {msg}"
            );
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

// ----- Cross-cutting invariants --------------------------------------------

#[test]
fn backend_hint_extension_policy_covers_recognized_set() {
    // Spec contract: tensorrt, libtorch, python_pytorch are the runtime-
    // supported v0.1.0 backends; vitis_ai and onnxruntime are reserved.
    for required in &[
        "tensorrt",
        "libtorch",
        "python_pytorch",
        "vitis_ai",
        "onnxruntime",
    ] {
        assert!(
            RECOGNIZED_BACKEND_HINTS.contains(required),
            "missing recognized backend hint `{required}`"
        );
    }
}

#[test]
fn parser_does_not_attempt_backend_fallback_on_unavailable_declared_backend() {
    // Bundle declares vitis_ai. Device offers libtorch + python_pytorch but
    // NOT vitis_ai. The runtime must not silently load the bundle through
    // an "available" backend — `evaluate_compatibility` rejects with
    // `unavailable_backend`, not with an "alternative chosen" success.
    let root = fixtures_root().join("vitis_synthetic");
    let d = parse_bundle(&root).expect("parse");
    let device = jetson_device(&[
        (
            "libtorch",
            BackendCapabilityView::default(),
            &["libtorch_state"],
        ),
        (
            "python_pytorch",
            BackendCapabilityView::default(),
            &["python_pytorch_entry"],
        ),
    ]);
    let r = evaluate_compatibility(&d, &device);
    assert!(!r.ok);
    assert!(
        r.violations
            .iter()
            .any(|v| v.code() == "unavailable_backend"),
        "violations did not include unavailable_backend: {:?}",
        r.violations
    );
}

#[test]
fn vision_fixture_passes_with_first_violation_short_circuit_semantics() {
    // The agent's verify() short-circuits on the first violation. The
    // compatibility evaluator emits violations in deterministic order so
    // unit tests can rely on the first slot for typed-error mapping.
    let root = fixtures_root().join("vision_tensorrt");
    let d = parse_bundle(&root).expect("parse");
    let device = DeviceContext {
        runtime_version: Some("0.1.0".into()),
        device_family: Some(DeviceFamily::JetsonOrin),
        device_memory_bytes: Some(64 * 1024 * 1024), // way too small
        backends: vec![BackendProfile {
            backend: "tensorrt".into(),
            capabilities: BackendCapabilityView {
                fixed_shape: true,
                ..BackendCapabilityView::default()
            },
            supported_precision: vec!["fp16".into()],
            supported_artifact_kinds: vec!["tensorrt_engine".into()],
        }],
    };
    let r = evaluate_compatibility(&d, &device);
    assert!(!r.ok);
    assert_eq!(r.violations[0].code(), "insufficient_memory");
}

#[test]
fn fixture_digests_are_deterministic_under_parser() {
    // Re-parsing the same bundle root must produce the same canonical
    // manifest digest; if a fixture artifact is touched without updating
    // the digest, the parser raises ArtifactDigestMismatch.
    let root = fixtures_root().join("vision_tensorrt");
    let a = parse_bundle(&root).expect("a");
    let b = parse_bundle(&root).expect("b");
    assert_eq!(a.manifest_digest, b.manifest_digest);
}

fn rules_root() -> PathBuf {
    fixtures_root().parent().unwrap().join("v0_2")
}

fn rule_manifest(fixture: &str) -> serde_json::Value {
    serde_json::from_str(
        &std::fs::read_to_string(rules_root().join(fixture).join("manifest.json")).unwrap(),
    )
    .unwrap()
}

fn parse_rule_change(
    fixture: &str,
    edit: impl FnOnce(&mut serde_json::Value),
) -> Result<tensorplate_protocol::BundleDescriptor, ParseError> {
    let source = rules_root().join(fixture);
    let copy = tempfile::tempdir().unwrap();
    let mut manifest = rule_manifest(fixture);
    for artifact in manifest["artifacts"].as_array().unwrap() {
        let path = artifact["path"].as_str().unwrap();
        let target = copy.path().join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(source.join(path), target).unwrap();
    }
    edit(&mut manifest);
    std::fs::write(copy.path().join("manifest.json"), manifest.to_string()).unwrap();
    parse_bundle(copy.path())
}

fn assert_rule(
    result: Result<tensorplate_protocol::BundleDescriptor, ParseError>,
    expected: tensorplate_protocol::BundleRuleCode,
) {
    let err = result.expect_err("rule must refuse");
    match err {
        ParseError::ManifestSemantics(error) => {
            assert_eq!(error.rule_code(), Some(expected), "{error}");
        }
        other => panic!("expected {expected}, got {other}"),
    }
}

#[test]
fn manifest_rule_fixture_pairs_retain_typed_reasons() {
    let read = |path: &Path| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    };
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let budget = read(&repo.join("config/schemas/memory_budget_breakdown.json"));
    let mut options = jsonschema::JSONSchema::options();
    options.with_document(budget["$id"].as_str().unwrap().to_owned(), budget.clone());
    let validator = options
        .compile(&read(&repo.join("protocol/schemas/bundle_manifest.json")))
        .unwrap();
    let mut counts = [0, 0];
    for entry in std::fs::read_dir(rules_root()).unwrap() {
        let path = entry.unwrap().path();
        if !path.join("expected.json").is_file() {
            continue;
        }
        let expected: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path.join("expected.json")).unwrap())
                .unwrap();
        match expected["rule"].as_str() {
            None => {
                assert!(
                    validator.is_valid(&read(&path.join("manifest.json"))),
                    "{}",
                    path.display()
                );
                parse_bundle(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                counts[0] += 1;
            }
            Some(code) => {
                match parse_bundle(&path).expect_err("reject fixture") {
                    ParseError::ManifestSemantics(e) => assert_eq!(
                        e.rule_code()
                            .map(tensorplate_protocol::BundleRuleCode::as_str),
                        Some(code),
                        "{}: {e}",
                        path.display()
                    ),
                    e => panic!("{}: expected {code}, got {e}", path.display()),
                }
                counts[1] += 1;
            }
        }
    }
    assert_eq!(counts, [11, 11]);
}

#[test]
fn manifest_rules_require_fields_only_in_their_applicable_profiles() {
    use tensorplate_protocol::BundleRuleCode as Rule;
    for key in [
        "support_level",
        "hardware_compatibility",
        "runner_profile",
        "compute_type",
        "pipeline_stages",
        "memory_budget_breakdown_bytes",
        "max_concurrent_sessions",
        "degraded_profile",
        "runtime_compatibility",
    ] {
        assert_rule(
            parse_rule_change("speech_stt_streaming", |m| {
                m.as_object_mut().unwrap().remove(key);
            }),
            Rule::RequiredField,
        );
    }
    assert_rule(
        parse_rule_change("speech_stt_streaming", |m| {
            m.as_object_mut().unwrap().remove("memory_budget_by_domain");
        }),
        Rule::StreamingState,
    );
    for version in ["0.2.1", "0.3", "0.3.bad", "bad", "0.3.0.1"] {
        assert_rule(
            parse_rule_change("speech_stt_streaming", |m| {
                m["runtime_compatibility"]["min_runtime_version"] = version.into();
            }),
            Rule::RequiredField,
        );
    }
    for version in ["0.3.0", "0.3.1", "0.4.0", "1.0.0"] {
        parse_rule_change("speech_stt_streaming", |m| {
            m["runtime_compatibility"]["min_runtime_version"] = version.into();
        })
        .unwrap();
    }
    for class in ["language", "embedding", "custom"] {
        assert_rule(
            parse_rule_change("valid_r11_vla_payload", |m| {
                m["model_class"] = class.into();
                m["model_blocks"] = serde_json::json!({class: {}});
            }),
            Rule::ReservedClass,
        );
    }
    for blocks in [
        serde_json::json!({}),
        serde_json::json!({"vision": {}}),
        serde_json::json!({"vla": {}, "language": {}}),
    ] {
        assert_rule(
            parse_rule_change("valid_r11_vla_payload", |m| m["model_blocks"] = blocks),
            Rule::ModelBlock,
        );
    }
    for key in [
        "profile_id",
        "backend_profile",
        "default_backend",
        "serving_mode",
    ] {
        assert_rule(
            parse_rule_change("speech_stt_streaming", |m| m[key] = serde_json::Value::Null),
            Rule::AmbiguousSelector,
        );
    }
    for mode in [
        "chunked_policy",
        "autoregressive_action_tokens",
        "flow_action_chunk",
        "hybrid_policy",
        "unknown",
    ] {
        assert_rule(
            parse_rule_change("valid_r11_vla_payload", |m| {
                m["model_blocks"]["vla"]["serving_mode"] = mode.into();
            }),
            Rule::ClassPayload,
        );
    }
    parse_rule_change("valid_r11_vla_payload", |m| {
        m.as_object_mut()
            .unwrap()
            .remove("memory_budget_breakdown_bytes");
    })
    .unwrap();
    assert_rule(
        parse_rule_change("valid_r11_vla_payload", |m| {
            m["support_level"] = "production".into();
            m["capability_requirements"] = serde_json::json!({});
            m.as_object_mut()
                .unwrap()
                .remove("memory_budget_breakdown_bytes");
        }),
        Rule::RequiredField,
    );
}

#[test]
fn manifest_rules_preserve_legacy_class_and_profile_behavior() {
    for class in ["language", "embedding", "custom", "vla", "vision"] {
        parse_rule_change("valid_r11_vla_payload", |m| {
            m["format_version"] = "0.1".into();
            m["model_class"] = class.into();
            m["model_blocks"] = serde_json::json!({class: {}});
            for key in [
                "support_level",
                "hardware_compatibility",
                "memory_budget_breakdown_bytes",
            ] {
                m.as_object_mut().unwrap().remove(key);
            }
            m["profile_id"] = "legacy".into();
            m["runner_profile"] = "future".into();
        })
        .unwrap();
    }
    parse_rule_change("valid_r11_vla_payload", |m| {
        m["format_version"] = "0.1".into();
        m["model_blocks"]["language"] = serde_json::json!({});
    })
    .unwrap();
}

#[test]
fn speech_precision_pairs_and_streaming_budget_boundaries() {
    use tensorplate_protocol::BundleRuleCode as Rule;
    let pairs = [
        ("float32", "fp32"),
        ("float16", "fp16"),
        ("bfloat16", "bfloat16"),
        ("int8", "int8"),
        ("int8_float16", "int8"),
        ("int8_float32", "int8"),
        ("int8_bfloat16", "int8"),
    ];
    for (compute, precision) in pairs {
        for hint in ["auto", "fp16", "fp32", "bfloat16", "int8", "int4"] {
            let result = parse_rule_change("speech_stt_streaming", |m| {
                m["compute_type"] = compute.into();
                m["precision_hint"] = hint.into();
            });
            if hint == precision {
                result.unwrap();
            } else {
                assert_rule(
                    result,
                    if hint == "auto" {
                        Rule::ExplicitPrecision
                    } else {
                        Rule::PrecisionConflict
                    },
                );
            }
        }
    }
    assert_rule(
        parse_rule_change("speech_stt_streaming", |m| {
            m["compute_type"] = "int16".into();
        }),
        Rule::PrecisionConflict,
    );
    assert_rule(
        parse_rule_change("speech_stt_streaming", |m| {
            m.as_object_mut().unwrap().remove("precision_hint");
        }),
        Rule::ExplicitPrecision,
    );
    parse_rule_change("invalid_r3_session_state", |m| {
        m["model_blocks"]["speech"]["serving_mode"] = "batch".into();
    })
    .unwrap();
    parse_rule_change("invalid_r3_session_state", |m| {
        m["memory_budget_by_domain"]["guest_ram"]["per_session_state_bytes"] = 1.into();
        m["memory_budget_breakdown_bytes"]["per_session_state_bytes"] = 1.into();
    })
    .unwrap();
    assert_rule(
        parse_rule_change("speech_stt_streaming", |m| {
            m["memory_budget_by_domain"]["device_vram"]["typo_bytes"] = 1.into();
        }),
        Rule::BudgetLine,
    );
}

#[test]
fn speech_required_fields_and_chunking_have_typed_reasons() {
    use tensorplate_protocol::BundleRuleCode as Rule;
    for fixture in ["speech_stt_streaming", "speech_tts_streaming"] {
        let task_fields = if fixture == "speech_stt_streaming" {
            vec!["input_audio_formats"]
        } else {
            vec!["voices", "output_audio_formats"]
        };
        for field in task_fields {
            assert_rule(
                parse_rule_change(fixture, |m| {
                    m["model_blocks"]["speech"]
                        .as_object_mut()
                        .unwrap()
                        .remove(field);
                }),
                Rule::RequiredField,
            );
        }
        for field in [
            "task",
            "serving_mode",
            "languages",
            "algorithm_profile",
            "quality_profile_digest",
            "benchmark_profile_digest",
        ] {
            assert_rule(
                parse_rule_change(fixture, |m| {
                    m["model_blocks"]["speech"]
                        .as_object_mut()
                        .unwrap()
                        .remove(field);
                }),
                Rule::RequiredField,
            );
        }
        for streaming in [true, false] {
            for missing in [true, false] {
                assert_rule(
                    parse_rule_change(fixture, |m| {
                        let speech = &mut m["model_blocks"]["speech"];
                        speech["serving_mode"] =
                            if streaming { "streaming" } else { "batch" }.into();
                        if missing {
                            speech.as_object_mut().unwrap().remove("chunking");
                        } else {
                            speech["chunking"] = serde_json::Value::Null;
                        }
                    }),
                    if streaming {
                        Rule::StreamingState
                    } else {
                        Rule::RequiredField
                    },
                );
            }
        }
        assert_rule(
            parse_rule_change(fixture, |m| {
                m["model_blocks"]["speech"]["serving_mode"] = "batch".into();
                m.as_object_mut().unwrap().remove("memory_budget_by_domain");
            }),
            Rule::RequiredField,
        );
    }
}

#[test]
fn production_requires_a_capability_declaration_and_class_blocks_cannot_be_null() {
    use tensorplate_protocol::BundleRuleCode as Rule;
    assert_rule(
        parse_rule_change("valid_r11_vla_payload", |m| {
            m["support_level"] = "production".into();
        }),
        Rule::RequiredField,
    );
    parse_rule_change("valid_r11_vla_payload", |m| {
        m["support_level"] = "production".into();
        m["capability_requirements"] = serde_json::json!({});
    })
    .unwrap();
    for block in ["vision", "speech", "language", "embedding", "custom", "vla"] {
        assert_rule(
            parse_rule_change("speech_stt_streaming", |m| {
                m["model_blocks"][block] = serde_json::Value::Null;
            }),
            Rule::ModelBlock,
        );
        parse_rule_change("valid_r11_vla_payload", |m| {
            m["format_version"] = "0.1".into();
            m["model_blocks"][block] = serde_json::Value::Null;
        })
        .unwrap();
    }
}

#[test]
fn speech_bundle_runtime_floor_is_independent_of_the_build_version() {
    for fixture in ["speech_stt_streaming", "speech_tts_streaming"] {
        let root = fixtures_root().parent().unwrap().join("v0_2").join(fixture);
        let bundle = parse_bundle(&root).expect("speech fixture parses");
        for (runtime, accepted) in [("0.2.1", false), ("0.3.0", true), ("0.3.1", true)] {
            let result = evaluate_compatibility(&bundle, &x86_64_python_pytorch_device(runtime));
            assert_eq!(result.ok, accepted, "{fixture} on {runtime}: {result:?}");
            if !accepted {
                assert_eq!(result.violations.len(), 1);
                assert_eq!(result.violations[0].code(), "unsupported_runtime");
            }
        }
    }
}

#[test]
fn only_exact_supported_bundle_formats_reach_payload_and_artifact_validation() {
    assert_eq!(
        tensorplate_protocol::SUPPORTED_BUNDLE_FORMAT_VERSIONS,
        ["0.1", "0.2"]
    );
    assert_eq!(tensorplate_protocol::BUNDLE_FORMAT_VERSION, "0.1");
    let source = fixtures_root()
        .parent()
        .unwrap()
        .join("v0_2/speech_stt_streaming");
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(source.join("manifest.json")).unwrap())
            .unwrap();
    let root = tempfile::tempdir().unwrap();
    for version in [
        "0.0", "0.3", "0.99", "1.0", "00.1", "0.01", "+0.1", "0.1.0", "", " 0.1", "0.1 ",
    ] {
        value["format_version"] = version.into();
        std::fs::write(root.path().join("manifest.json"), value.to_string()).unwrap();
        let err = parse_bundle(root.path()).expect_err("unsupported format");
        assert!(
            matches!(err, ParseError::UnsupportedFormatVersion { ref got, supported }
            if got == version && supported == ["0.1", "0.2"]),
            "{err}"
        );
    }
    for fixture in [fixtures_root().join("vision_tensorrt"), source] {
        parse_bundle(&fixture).expect("a supported format still parses");
    }
}

#[test]
fn generic_manifest_reader_cannot_bypass_the_format_allowlist() {
    let source = fixtures_root().join("vision_tensorrt/manifest.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(source).unwrap()).unwrap();
    for version in ["0.0", "0.3", "0.99", "1.0"] {
        value["format_version"] = version.into();
        let error = tensorplate_protocol::decode_with_version_check::<
            tensorplate_protocol::BundleManifest,
        >(&value.to_string())
        .expect_err("unsupported format");
        assert!(matches!(
            error,
            tensorplate_protocol::DecodeError::InvalidPayload(_)
        ));
        assert!(error.to_string().contains("format_version"));
    }
    value["format_version"] = "0.1".into();
    tensorplate_protocol::decode_with_version_check::<tensorplate_protocol::BundleManifest>(
        &value.to_string(),
    )
    .unwrap();
}
