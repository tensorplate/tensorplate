// SPDX-License-Identifier: Apache-2.0
//
// The bundle rules a manifest cannot be judged on alone, deployed through
// the coordinator on an agent that holds the facts they need: the committed
// platform rows, a machine admitted on a Preview row, both speech runner
// profiles installed, and the lineage base bundle deployed.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access
)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use common::Harness;
use tensorplate_agent::backend_detection::{
    BackendProbeReport, BackendProbeState, RunnerProfileProbe,
};
use tensorplate_agent::config::AgentConfig;
use tensorplate_agent::coordinator::Coordinator;
use tensorplate_agent::error::AgentError;
use tensorplate_agent::platform_admission::PlatformAdmission;
use tensorplate_platform::{AdmissionPosture, PlatformRegistry, SupportLevel};
use tensorplate_protocol::backend_descriptor::{ComputeType, RunnerProfile};
use tensorplate_protocol::bundle::parse_bundle;
use tensorplate_protocol::ErrorCode;

const PRODUCTION_ROW: &str = "ubuntu2404-x86-l4-g2s8";
const PREVIEW_ROW: &str = "ubuntu2404-x86-l4-g2s24";
const BASE: &str = "valid_r8_base_bundle";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn fixtures() -> PathBuf {
    repo().join("test/models/bundles/v0_2")
}

fn registry() -> PlatformRegistry {
    let registry = PlatformRegistry::load(&repo().join("config/platform")).expect("registry");
    // What the fixtures rest on: they name rows by the level each holds.
    for (row, level) in [
        (PRODUCTION_ROW, SupportLevel::Production),
        ("ubuntu2404-x86-h100-80g-a3hg1", SupportLevel::Production),
        (PREVIEW_ROW, SupportLevel::Preview),
    ] {
        assert_eq!(registry.row(row).unwrap().support_level(), level, "{row}");
    }
    assert!(registry.row("ubuntu2404-x86-l4-unlisted").is_none());
    registry
}

fn admitted_on(registry: &PlatformRegistry, row_id: &str, validated: bool) -> PlatformAdmission {
    let row = registry.row(row_id).unwrap();
    let installed_packages: BTreeSet<String> = row
        .backend_packages()
        .iter()
        .flat_map(|set| set.packages.iter().cloned())
        .collect();
    PlatformAdmission::Supported {
        row_id: row_id.to_owned(),
        capability: None,
        installed_packages,
        posture: AdmissionPosture::TechnicalPrerequisites,
        posture_from: "row",
        validated,
        memory_telemetry: None,
        signal_telemetry: None,
    }
}

fn installed_runner(id: &str, compute_types: &[ComputeType]) -> RunnerProfileProbe {
    RunnerProfileProbe {
        profile: RunnerProfile {
            id: id.into(),
            interpreter: "/opt/env/bin/python".into(),
            environment_root: "/opt/env".into(),
            library_search_paths: Vec::new(),
            packages: vec!["speech-runtime".into()],
            compute_types: compute_types.to_vec(),
        },
        state: BackendProbeState::Runnable,
        observed: None,
    }
}

fn probes() -> BTreeMap<String, BackendProbeReport> {
    BTreeMap::from([(
        "python_pytorch".to_string(),
        BackendProbeReport {
            backend_name: "python_pytorch".into(),
            descriptor_path: PathBuf::from("backend.json"),
            state: BackendProbeState::Runnable,
            install_hint: None,
            runner_profiles: vec![
                installed_runner(
                    "faster_whisper",
                    &[ComputeType::Float16, ComputeType::Float32],
                ),
                installed_runner("kokoro", &[ComputeType::Float32]),
            ],
        },
    )])
}

fn config(h: &Harness) -> AgentConfig {
    let mut config = h.config.clone();
    config.runtime_version = Some("0.3.1".into());
    config.device_family = tensorplate_protocol::DeviceFamily::X86_64;
    let mut caps = config.backend_capabilities["mock"].clone();
    caps.supported_artifact_kinds = vec!["python_pytorch_entry".into()];
    caps.supported_precision.push("int8".into());
    for backend in ["python_pytorch", "tensorrt"] {
        config.available_backends.push(backend.into());
        config
            .backend_capabilities
            .insert(backend.into(), caps.clone());
    }
    config
}

/// An agent that holds every fact, admitted on `row`.
fn agent(h: &Harness, row: &str, validated: bool) -> Coordinator {
    let registry = registry();
    let admission = admitted_on(&registry, row, validated);
    Coordinator::new(config(h), h.store.clone(), h.worker.clone())
        .with_backend_probes(probes())
        .with_platform_registry(registry)
        .with_platform_admission(admission)
}

fn deploy(coord: &Coordinator, id: &str, bundle: &Path) -> Result<(), AgentError> {
    coord
        .deploy(id, bundle, Default::default(), None, None)
        .map(|_| ())
}

/// A copy of the bundle at `source` under `root`, its manifest edited.
fn edited_copy(root: &Path, source: &Path, edit: &dyn Fn(&mut serde_json::Value)) -> PathBuf {
    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let dir = root.join(source.file_name().unwrap());
    copy_tree(source, &dir);
    let text = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
    let mut manifest: serde_json::Value = serde_json::from_str(&text).unwrap();
    edit(&mut manifest);
    std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
    dir
}

fn expected(fixture: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(fixture.join("expected.json")).ok()?;
    Some(serde_json::from_str(&text).unwrap())
}

/// The code a deploy of the fixture returns: `deploy_code` where the
/// agent's facts change the parser's verdict, else the parser's `rule`.
fn deploy_code(expected: &serde_json::Value) -> Option<String> {
    expected
        .get("deploy_code")
        .unwrap_or(&expected["rule"])
        .as_str()
        .map(str::to_owned)
}

#[test]
fn every_rule_fixture_gets_its_code_from_deploy_before_anything_is_staged() {
    let h = Harness::new();
    let coord = agent(&h, PREVIEW_ROW, true);
    deploy(&coord, "base", &fixtures().join(BASE)).expect("the base bundle deploys");
    let calls = h.worker.calls().unwrap().len();
    assert!(calls > 0);

    let mut refused = BTreeMap::new();
    let mut accepted = Vec::new();
    for entry in std::fs::read_dir(fixtures()).unwrap() {
        let path = entry.unwrap().path();
        let Some(expected) = expected(&path) else {
            continue;
        };
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let Some(code) = deploy_code(&expected) else {
            accepted.push(path);
            continue;
        };
        let error = deploy(&coord, "refused", &path).expect_err(&name);
        let record = error.to_record();
        assert_eq!(record.context.as_deref(), Some(code.as_str()), "{name}");
        assert!(!record.recoverable, "{name}");
        if code.starts_with("bundle_r") {
            let AgentError::BundleManifest(ref failure) = error else {
                panic!("{name}: {error}");
            };
            assert_eq!(failure.rule.unwrap().as_str(), code, "{name}");
            assert_eq!(record.code, ErrorCode::ConfigInvalid, "{name}");
        } else {
            assert_eq!(record.code, ErrorCode::Unsupported, "{name}");
        }
        assert!(
            !h.config.staging_dir.join("refused").exists(),
            "{name} must never stage"
        );
        assert_eq!(h.worker.calls().unwrap().len(), calls, "{name}");
        let snapshot = h.store.snapshot().unwrap();
        assert_eq!(snapshot.active.unwrap().deployment_id, "base", "{name}");
        let quarantined = snapshot.quarantined.last().unwrap().error.clone();
        assert_eq!(
            quarantined.context.as_deref(),
            Some(code.as_str()),
            "{name}"
        );
        refused.insert(name, code);
    }
    assert_eq!(refused.len(), 24);

    // The codes only this agent's facts produce, by fixture.
    for (fixture, code) in [
        ("invalid_r6_compute_type", "bundle_r6_compute_type"),
        ("invalid_r6_runner_selector", "bundle_r6_runner_selector"),
        ("invalid_r6_runner_not_installed", "missing_backend_package"),
        ("invalid_r8_unresolved_base", "bundle_r8_base_reference"),
        (
            "invalid_r8_support_above_row",
            "bundle_r8_variant_support_level",
        ),
        ("invalid_r8_reserved_adapter", "bundle_r8_reserved_variant"),
        ("invalid_r9_unknown_row", "bundle_r9_hardware_row"),
        ("invalid_r9_production_claim", "bundle_r9_support_claim"),
    ] {
        assert_eq!(refused[fixture], code, "{fixture}");
    }

    assert_eq!(accepted.len(), 14);
    for path in accepted {
        let h = Harness::new();
        let coord = agent(&h, PREVIEW_ROW, true);
        deploy(&coord, "accepted", &path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(
            h.store.snapshot().unwrap().active.unwrap().deployment_id,
            "accepted"
        );
    }
}

#[test]
fn the_parser_accepts_what_only_the_target_can_refuse() {
    for fixture in [
        "invalid_r6_compute_type",
        "invalid_r6_runner_selector",
        "invalid_r6_runner_not_installed",
        "invalid_r9_unknown_row",
        "invalid_r9_production_claim",
    ] {
        parse_bundle(&fixtures().join(fixture)).unwrap_or_else(|e| panic!("{fixture}: {e}"));
    }
}

fn refusal_code(coord: &Coordinator, fixture: &str) -> String {
    deploy(coord, "refused", &fixtures().join(fixture))
        .expect_err(fixture)
        .to_record()
        .context
        .unwrap_or_else(|| panic!("{fixture}: no code"))
}

#[test]
fn a_variant_resolves_only_against_a_bundle_deployed_on_a_row_the_machine_holds() {
    let variant = "invalid_r8_reserved_speaker_embedding";

    // Nothing deployed: the base is not known.
    let h = Harness::new();
    let coord = agent(&h, PREVIEW_ROW, true);
    assert_eq!(refusal_code(&coord, variant), "bundle_r8_base_reference");
    deploy(&coord, "base", &fixtures().join(BASE)).unwrap();
    assert_eq!(refusal_code(&coord, variant), "bundle_r8_reserved_variant");

    // Admitted on technical prerequisites: the row's evidence does not
    // cover the machine, so the deployed base holds no level.
    let h = Harness::new();
    let coord = agent(&h, PREVIEW_ROW, false);
    deploy(&coord, "base", &fixtures().join(BASE)).unwrap();
    assert_eq!(refusal_code(&coord, variant), "bundle_r8_base_reference");

    // No platform facts at all.
    let h = Harness::new();
    let coord = Coordinator::new(config(&h), h.store.clone(), h.worker.clone());
    deploy(&coord, "base", &fixtures().join(BASE)).unwrap();
    assert_eq!(refusal_code(&coord, variant), "bundle_r8_base_reference");
}

#[test]
fn a_variant_is_held_to_the_level_of_the_row_and_not_of_the_manifests() {
    let above = "invalid_r8_support_above_row";
    let within = "invalid_r8_support_escalation";

    let h = Harness::new();
    let coord = agent(&h, PREVIEW_ROW, true);
    deploy(&coord, "base", &fixtures().join(BASE)).unwrap();
    assert_eq!(
        refusal_code(&coord, above),
        "bundle_r8_variant_support_level"
    );
    assert_eq!(refusal_code(&coord, within), "bundle_r8_reserved_variant");

    let h = Harness::new();
    let coord = agent(&h, PRODUCTION_ROW, true);
    deploy(&coord, "base", &fixtures().join(BASE)).unwrap();
    assert_eq!(refusal_code(&coord, above), "bundle_r8_reserved_variant");

    // A base whose own manifest asks for `production` is still known at the
    // row's level.
    let td = tempfile::TempDir::new().unwrap();
    let base = edited_copy(td.path(), &fixtures().join(BASE), &|manifest| {
        manifest["support_level"] = "production".into();
        manifest["capability_requirements"] = serde_json::json!({});
    });
    let digest = parse_bundle(&base).unwrap().manifest_digest;
    let variant = edited_copy(td.path(), &fixtures().join(above), &|manifest| {
        manifest["base_model_ref"]["manifest_digest"] = digest.clone().into();
    });
    let h = Harness::new();
    let coord = agent(&h, PREVIEW_ROW, true);
    deploy(&coord, "base", &base).unwrap();
    assert_eq!(
        deploy(&coord, "refused", &variant)
            .unwrap_err()
            .to_record()
            .context
            .as_deref(),
        Some("bundle_r8_variant_support_level")
    );
}

#[test]
fn a_lineage_failure_is_reported_before_the_manifests_own_rules() {
    let td = tempfile::TempDir::new().unwrap();
    let source = fixtures().join("invalid_r8_unresolved_base");
    let also_incomplete = edited_copy(td.path(), &source, &|manifest| {
        manifest.as_object_mut().unwrap().remove("degraded_profile");
    });
    match parse_bundle(&also_incomplete) {
        Err(tensorplate_protocol::bundle::ParseError::ManifestSemantics(e)) => assert_eq!(
            e.rule_code()
                .map(tensorplate_protocol::BundleRuleCode::as_str),
            Some("bundle_r12_required_field")
        ),
        other => panic!("{other:?}"),
    }
    let h = Harness::new();
    let coord = agent(&h, PREVIEW_ROW, true);
    deploy(&coord, "base", &fixtures().join(BASE)).unwrap();
    assert_eq!(
        deploy(&coord, "refused", &also_incomplete)
            .unwrap_err()
            .to_record()
            .context
            .as_deref(),
        Some("bundle_r8_base_reference")
    );
}

#[test]
fn format_0_1_bundles_are_not_judged_by_these_rules() {
    let td = tempfile::TempDir::new().unwrap();
    let source = repo().join("test/models/bundles/v0_1/x86_fixture_smoke");
    // Under format 0.1 these keys are extras the reader ignores.
    let bundle = edited_copy(td.path(), &source, &|manifest| {
        manifest["base_model_ref"] = serde_json::json!({
            "name": "absent", "version": "1", "manifest_digest": format!("sha256:{}", "0".repeat(64))
        });
        manifest["variant_identity"] = serde_json::json!({
            "id": "v", "revision": "1", "variant_kind": "adapter"
        });
        manifest["hardware_compatibility"] = serde_json::json!(["ubuntu2404-x86-l4-unlisted"]);
        manifest["support_level"] = "experimental".into();
        manifest["runner_profile"] = "whisper_cpp".into();
        manifest["compute_type"] = "int8".into();
    });
    // The same keys are a variant declaration to the format 0.2 decoder.
    let text = std::fs::read_to_string(bundle.join("manifest.json")).unwrap();
    let as_0_2 = tensorplate_protocol::BundleProfile::from_manifest_text(&text).unwrap();
    assert!(as_0_2.lineage.is_some());
    let h = Harness::new();
    let coord = agent(&h, PREVIEW_ROW, true);
    deploy(&coord, "legacy", &bundle).expect("a format 0.1 bundle deploys as before");
}

#[test]
fn hardware_rows_are_judged_only_where_a_registry_is_loaded() {
    let h = Harness::new();
    let coord = Coordinator::new(config(&h), h.store.clone(), h.worker.clone());
    for fixture in ["invalid_r9_unknown_row", "invalid_r9_production_claim"] {
        deploy(&coord, "unjudged", &fixtures().join(fixture)).expect(fixture);
    }
}
