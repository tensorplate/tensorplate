// SPDX-License-Identifier: Apache-2.0
//
// bundle format: bundle conformance suite.
//
// These tests assert the v0.1.0 bundle contract end-to-end against the
// checked-in fixtures under `test/models/bundles/v0_1/`. They run on
// the host without TensorRT, CUDA, PyTorch, or Vitis AI SDKs — the
// parser/verifier path is deliberately SDK-free in v0.1.0.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

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
