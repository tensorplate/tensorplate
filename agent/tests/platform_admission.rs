// SPDX-License-Identifier: Apache-2.0
//
// Deploy admission against the real committed registry.
//
// The rejections here are the ones that must happen before anything is
// staged: a partitioned accelerator, a driver stack that does not match
// what the matched row records, and a backend whose packages are not
// installed. Every requirement is read from the row — none is written
// down twice, here or in the agent.

#![allow(clippy::expect_used, clippy::panic)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use tensorplate_agent::error::AgentError;
use tensorplate_agent::platform_admission::{
    check_backend_packages, ObservedStack, PlatformAdmission,
};
use tensorplate_platform::{
    identify_accelerator, AcceleratorIdentity, AcceleratorObservation, AcceleratorSources,
    DetectedArchitecture, DetectedVendor, ExactHostFacts, HostIdentity, HostReport, PlatformReason,
    PlatformRegistry, PlatformReport, PlatformSupportRow,
};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn registry() -> PlatformRegistry {
    PlatformRegistry::load(&repo_root().join("config/platform")).expect("registry loads")
}

/// The A100 row, which is the one the MIG fixtures are written against.
fn a100(registry: &PlatformRegistry) -> &PlatformSupportRow {
    registry
        .row("ubuntu2404-x86-a100-40g-a2hg1")
        .expect("the A100 row is committed")
}

fn host_of(row: &PlatformSupportRow) -> HostIdentity {
    let cpu = row.cpu();
    HostIdentity {
        architecture: DetectedArchitecture::Known(cpu.architecture),
        vendor: DetectedVendor::Known(*cpu.vendors.first().expect("a row names a vendor")),
        os_name: row.os().name.clone(),
        os_version: row.os().version.clone(),
        image_identity: row.os().image_identity.clone(),
        machine_type: row.validation_environment().machine_type.clone(),
    }
}

/// Wrap a host identity and an accelerator into the observation shape
/// admission consumes. The memory figures are the row's own, so a case
/// only ever varies the dimension it is about.
fn report_of(
    row: &PlatformSupportRow,
    host: HostIdentity,
    accelerator: Option<AcceleratorIdentity>,
) -> PlatformReport {
    PlatformReport {
        host: HostReport {
            identity: host,
            exact: ExactHostFacts::default(),
        },
        accelerator: accelerator.map(|identity| {
            let declared = row.accelerator().expect("a row with an accelerator");
            AcceleratorObservation {
                identity,
                memory_bytes: Some(declared.memory_bytes),
                memory_profile: declared.memory_profile,
            }
        }),
    }
}

/// Detect from one of the committed accelerator fixtures, so admission is
/// driven by the same recorded text detection is.
fn detected_from_fixture(row: &PlatformSupportRow, fixture: &str) -> PlatformReport {
    let text = std::fs::read_to_string(
        repo_root()
            .join("test/platform/accelerator")
            .join(format!("{fixture}.txt")),
    )
    .expect("read fixture");
    let report = identify_accelerator(&AcceleratorSources {
        nvidia_smi_query: Some(text),
    })
    .expect("detection succeeds")
    .expect("one device");
    report_of(row, host_of(row), Some(report.identity))
}

#[test]
fn a_partitioned_accelerator_is_refused_with_mig_mode_enabled() {
    // The card is supported and the host is supported; only the
    // partitioning is wrong. Serving it anyway would serve at a capacity
    // the row's evidence was never collected at.
    let registry = registry();
    let detected = detected_from_fixture(a100(&registry), "mig-enabled-a100-40g");
    match PlatformAdmission::evaluate(&registry, &detected, &ObservedStack::default(), None) {
        PlatformAdmission::Rejected {
            reason: Some(reason),
            detail,
            ..
        } => {
            assert_eq!(reason, PlatformReason::MigModeEnabled);
            assert!(!detail.is_empty(), "a rejection must say why");
        }
        other => panic!("a partitioned accelerator must be refused, got {other:?}"),
    }
}

#[test]
fn the_same_card_unpartitioned_is_admitted() {
    // The control for the case above. Without it, a check that refused
    // everything would look correct.
    let registry = registry();
    let detected = detected_from_fixture(a100(&registry), "ubuntu2404-x86-a100-40g-a2hg1");
    match PlatformAdmission::evaluate(&registry, &detected, &ObservedStack::default(), None) {
        PlatformAdmission::Supported { row_id, .. } => {
            assert_eq!(row_id, "ubuntu2404-x86-a100-40g-a2hg1");
        }
        other @ PlatformAdmission::Rejected { .. } => {
            panic!("a non-partitioned supported card must be admitted, got {other:?}")
        }
    }
}

#[test]
fn an_unknown_discrete_framebuffer_uses_the_row_budget_without_rejecting_capacity() {
    let registry = registry();
    let row = registry
        .row("ubuntu2404-x86-l4-g2s8")
        .expect("the L4 row is committed");
    let declared = row.accelerator().expect("the L4 row has an accelerator");
    let report = PlatformReport {
        host: HostReport {
            identity: host_of(row),
            exact: ExactHostFacts::default(),
        },
        accelerator: Some(AcceleratorObservation {
            identity: AcceleratorIdentity {
                sku: declared.sku.clone(),
                partitioned: false,
            },
            memory_bytes: None,
            memory_profile: declared.memory_profile,
        }),
    };

    let admission =
        PlatformAdmission::evaluate(&registry, &report, &ObservedStack::default(), None);
    let PlatformAdmission::Supported {
        capability: Some(capability),
        ..
    } = &admission
    else {
        panic!("a supported L4 with an unreadable framebuffer must remain admitted");
    };
    assert_eq!(capability.detected_memory_bytes(), None);
    assert_eq!(
        capability.max_resident_model_memory(),
        declared.memory_bytes,
        "an absent reading adds no tighter bound than the validated row budget"
    );

    let harness = common::Harness::new();
    let mut config = harness.config.clone();
    config.device_memory_bytes = None;
    admission.apply_memory_limit(&mut config);
    assert_eq!(config.device_memory_bytes, Some(declared.memory_bytes));

    let bundle = common::write_bundle(
        harness.td.path(),
        "unknown-framebuffer",
        common::BundleSpec {
            memory_estimate_bytes: Some(1024 * 1024 * 1024),
            ..Default::default()
        },
    );
    tensorplate_agent::bundle::verify(&bundle, &config)
        .expect("a positive estimate below the row budget must remain admissible");
}

#[test]
fn an_off_matrix_card_is_refused_with_the_sku_reason() {
    let registry = registry();
    let detected = detected_from_fixture(a100(&registry), "unsupported-a100-80gb");
    match PlatformAdmission::evaluate(&registry, &detected, &ObservedStack::default(), None) {
        PlatformAdmission::Rejected {
            reason: Some(reason),
            ..
        } => {
            assert_eq!(reason, PlatformReason::UnsupportedAcceleratorSku);
        }
        other => panic!("an off-matrix card must be refused, got {other:?}"),
    }
}

#[test]
fn a_driver_stack_requirement_is_read_from_the_row_not_from_here() {
    // The committed rows record no components yet — those are captured at
    // the first evidence run — so the requirement under test is taken from
    // a row that does declare one. That is the point: the check has no
    // version of its own to compare against.
    let registry = registry();
    let row = a100(&registry);
    let detected = detected_from_fixture(row, "ubuntu2404-x86-a100-40g-a2hg1");

    // A row with no recorded stack must not invent a requirement.
    assert!(
        row.kernel_driver_stack().components.is_empty(),
        "this case assumes the committed row has no recorded stack yet"
    );
    assert!(matches!(
        PlatformAdmission::evaluate(&registry, &detected, &ObservedStack::default(), None),
        PlatformAdmission::Supported { .. }
    ));

    // And a row that does record one is enforced against what the machine
    // reports. Exercised through a registry loaded from a row carrying
    // components, so the requirement is genuinely row-sourced.
    let staged = staged_registry_with_components(&[("nvidia_driver", "550.54.15")]);
    let detected = detected_from_fixture(a100(&staged), "ubuntu2404-x86-a100-40g-a2hg1");

    let matching = ObservedStack {
        components: BTreeMap::from([("nvidia_driver".to_string(), "550.54.15".to_string())]),
        installed_packages: BTreeSet::new(),
    };
    assert!(matches!(
        PlatformAdmission::evaluate(&staged, &detected, &matching, None),
        PlatformAdmission::Supported { .. }
    ));

    for observed in [
        ObservedStack::default(),
        ObservedStack {
            components: BTreeMap::from([("nvidia_driver".to_string(), "535.104.5".to_string())]),
            installed_packages: BTreeSet::new(),
        },
    ] {
        match PlatformAdmission::evaluate(&staged, &detected, &observed, None) {
            PlatformAdmission::Rejected {
                reason: Some(reason),
                detail,
                ..
            } => {
                assert_eq!(reason, PlatformReason::MissingDriverRuntime);
                assert!(
                    detail.contains("nvidia_driver") && detail.contains("550.54.15"),
                    "the rejection must name the component and the version the row records: {detail}"
                );
            }
            other => panic!("a mismatched driver stack must be refused, got {other:?}"),
        }
    }
}

/// A registry loaded from the committed rows with the A100 row's
/// `kernel_driver_stack` replaced, so a stack requirement can be exercised
/// before any evidence run has recorded one.
fn staged_registry_with_components(components: &[(&str, &str)]) -> PlatformRegistry {
    let staging = stage_registry();
    let row_path = staging.join("rows/ubuntu2404-x86-a100-40g-a2hg1.json");
    let mut row: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&row_path).expect("read row"))
            .expect("row parses");
    row["kernel_driver_stack"]["components"] = serde_json::Value::Array(
        components
            .iter()
            .map(|(component, version)| {
                serde_json::json!({ "component": component, "version": version })
            })
            .collect(),
    );
    std::fs::write(
        &row_path,
        serde_json::to_string_pretty(&row).expect("serialize"),
    )
    .expect("write row");

    PlatformRegistry::load(&staging).expect("staged registry loads")
}

/// A writable copy of the committed registry, so a case can exercise a row
/// shape no committed row declares yet.
fn stage_registry() -> PathBuf {
    let staging = std::env::temp_dir().join(format!(
        "tp-admission-registry-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&staging);
    copy_dir(&repo_root().join("config/platform"), &staging);
    staging
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("create dir");
    for entry in std::fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("dir entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

#[test]
fn a_backend_path_the_row_declares_requires_its_packages() {
    let registry = registry();
    let row = a100(&registry);

    // The row declares tensorplate-backend-python-pytorch for this path.
    let installed = BTreeSet::from(["tensorplate-backend-python-pytorch".to_string()]);
    assert!(check_backend_packages(row, "python_pytorch", &installed).is_ok());

    match check_backend_packages(row, "python_pytorch", &BTreeSet::new()) {
        Err(AgentError::PlatformNotAdmissible { reason, detail }) => {
            assert_eq!(reason, Some(PlatformReason::MissingBackendPackage));
            assert!(
                detail.contains("tensorplate-backend-python-pytorch"),
                "the rejection must name the package the row requires: {detail}"
            );
        }
        other => panic!("a missing backend package must be refused, got {other:?}"),
    }
}

#[test]
fn a_backend_path_the_row_never_claimed_is_refused_rather_than_waved_through() {
    // A row that declares no package set for a path is not a row with
    // nothing to require — it is a row that never claimed to serve that
    // path at all. Treating an absent set as "no requirement" would admit
    // exactly the deploy that has no evidence behind it.
    let registry = registry();
    let row = a100(&registry);
    match check_backend_packages(row, "vitis_ai", &BTreeSet::new()) {
        Err(AgentError::PlatformNotAdmissible { reason, detail }) => {
            assert_eq!(reason, Some(PlatformReason::MissingBackendPackage));
            assert!(detail.contains("vitis_ai"), "names the path: {detail}");
        }
        other => panic!("an undeclared backend path must be refused, got {other:?}"),
    }
}

#[test]
fn a_rejected_machine_stays_rejected_whatever_backend_a_bundle_names() {
    // The startup verdict is not something a bundle can talk its way past.
    let registry = registry();
    let detected = detected_from_fixture(a100(&registry), "mig-enabled-a100-40g");
    let admission = PlatformAdmission::evaluate(
        &registry,
        &detected,
        &ObservedStack {
            components: BTreeMap::new(),
            installed_packages: BTreeSet::from(["tensorplate-backend-python-pytorch".to_string()]),
        },
        None,
    );

    for backend in ["python_pytorch", "tensorrt", "anything"] {
        match admission.admit_backend(&registry, backend) {
            Err(AgentError::PlatformNotAdmissible { reason, .. }) => assert_eq!(
                reason,
                Some(PlatformReason::MigModeEnabled),
                "the machine-level reason must survive, not be replaced by a package one"
            ),
            Ok(()) => panic!("a partitioned machine must not admit `{backend}`"),
            Err(other) => panic!("expected a platform rejection, got {other:?}"),
        }
    }
}

#[test]
fn a_machine_shape_miss_carries_no_borrowed_reason() {
    // The frozen vocabulary has no value for "wrong machine shape", and
    // the nearest candidates all name a dimension that is fine. An
    // operator told their OS version is unsupported, when their OS is
    // correct and their chassis is not, goes and reinstalls the wrong
    // thing.
    let registry = registry();
    let row = a100(&registry);
    let mut host = host_of(row);
    host.machine_type = Some("a2-ultragpu-1g".to_string());
    let report = identify_accelerator(&AcceleratorSources {
        nvidia_smi_query: Some(
            std::fs::read_to_string(
                repo_root().join("test/platform/accelerator/ubuntu2404-x86-a100-40g-a2hg1.txt"),
            )
            .expect("read fixture"),
        ),
    })
    .expect("detection succeeds")
    .expect("one device");
    let detected = report_of(row, host, Some(report.identity));

    match PlatformAdmission::evaluate(&registry, &detected, &ObservedStack::default(), None) {
        PlatformAdmission::Rejected {
            reason: None,
            ref detail,
            ..
        } => {
            assert!(
                detail.contains("machine shape") || detail.contains("validated environment"),
                "the detail must say what is actually wrong: {detail}"
            );
        }
        other => {
            panic!("a host on an uncovered machine shape must be refused as such, got {other:?}")
        }
    }
}

#[test]
fn an_experimental_row_does_not_borrow_the_planned_reason() {
    // `registry.rs` states that Experimental is deliberately not reported
    // as Planned, because the frozen reason for Planned means a row
    // awaiting hardware validation — which an Experimental integration is
    // not, and never will be. Borrowing it tells an operator to wait for
    // an evidence run that is not coming.
    let staged = staged_registry_with_support_level("Experimental");
    let row = staged
        .row("ubuntu2404-x86-a100-40g-a2hg1")
        .expect("staged row");
    let detected = detected_from_fixture(row, "ubuntu2404-x86-a100-40g-a2hg1");

    match PlatformAdmission::evaluate(&staged, &detected, &ObservedStack::default(), None) {
        rejected @ PlatformAdmission::Rejected { .. } => {
            assert_eq!(
                rejected.reason(),
                None,
                "an Experimental row must not borrow a frozen reason, got {rejected:?}"
            );
            let PlatformAdmission::Rejected { ref detail, .. } = rejected else {
                unreachable!("matched on Rejected")
            };
            assert!(
                detail.contains("experimental"),
                "the detail must say what is actually true: {detail}"
            );
        }
        other @ PlatformAdmission::Supported { .. } => {
            panic!("an Experimental row is not a supported combination, got {other:?}")
        }
    }
}

/// The committed registry with the A100 row's `support_level` replaced, so
/// a level no row currently declares can still be exercised.
fn staged_registry_with_support_level(level: &str) -> PlatformRegistry {
    let staging = stage_registry();
    let row_path = staging.join("rows/ubuntu2404-x86-a100-40g-a2hg1.json");
    let mut row: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&row_path).expect("read row"))
            .expect("row parses");
    row["support_level"] = serde_json::Value::String(level.to_string());
    std::fs::write(
        &row_path,
        serde_json::to_string_pretty(&row).expect("serialize"),
    )
    .expect("write row");
    PlatformRegistry::load(&staging).expect("staged registry loads")
}

#[test]
fn the_typed_reason_reaches_the_error_record_a_caller_can_read() {
    // The acceptance criterion is that these rejections are TYPED. The
    // Display impl renders only the prose, so without projecting the
    // reason into the record the spellings live inside this crate and
    // nowhere a CLI or the durable store can see them — which would make
    // the criterion true of an internal enum and false of the contract
    // that actually reports it.
    use tensorplate_agent::error::AgentError;

    for reason in [
        PlatformReason::MigModeEnabled,
        PlatformReason::MissingDriverRuntime,
        PlatformReason::MissingBackendPackage,
        PlatformReason::UnsupportedAcceleratorSku,
    ] {
        let record = AgentError::PlatformNotAdmissible {
            reason: Some(reason),
            detail: "detail".to_string(),
        }
        .to_record();
        assert_eq!(
            record.context.as_deref(),
            Some(reason.as_str()),
            "the typed reason must be readable off the record, not only in prose"
        );
    }

    // A refusal the frozen vocabulary has no value for carries no reason,
    // and must not invent one to fill the slot.
    let record = AgentError::PlatformNotAdmissible {
        reason: None,
        detail: "machine shape".to_string(),
    }
    .to_record();
    assert_eq!(record.context, None);
}

#[test]
fn the_coordinator_actually_refuses_a_deploy_on_a_rejected_machine() {
    // Everything above tests `evaluate_platform` directly. The acceptance
    // criterion is about admission at DEPLOY, so without this the wiring
    // could be absent and every case above would still pass — the verdict
    // would simply never be consulted.
    use std::sync::Arc;
    use tensorplate_agent::coordinator::Coordinator;
    use tensorplate_agent::error::AgentError;

    let harness = common::Harness::new();
    let registry = registry();
    let detected = detected_from_fixture(a100(&registry), "mig-enabled-a100-40g");
    let verdict =
        PlatformAdmission::evaluate(&registry, &detected, &ObservedStack::default(), None);
    assert!(matches!(verdict, PlatformAdmission::Rejected { .. }));

    let coordinator = Arc::new(
        Coordinator::new(
            harness.config.clone(),
            harness.store.clone(),
            harness.worker.clone(),
        )
        .with_platform_admission(verdict)
        .with_platform_registry(registry),
    );

    let bundle = common::write_bundle(
        harness.td.path(),
        "mig",
        common::BundleSpec {
            backend_hint: Some("python_pytorch"),
            ..Default::default()
        },
    );

    match coordinator.deploy("d-mig", &bundle, BTreeMap::default(), None, None) {
        Err(AgentError::PlatformNotAdmissible { reason, detail }) => {
            assert_eq!(
                reason,
                Some(PlatformReason::MigModeEnabled),
                "the deploy path must carry the machine-level reason through"
            );
            assert!(!detail.is_empty());
        }
        other => panic!("a partitioned machine must not deploy, got {other:?}"),
    }
}

#[test]
fn the_coordinator_applies_backend_admission_after_bundle_verification() {
    use std::sync::Arc;
    use tensorplate_agent::coordinator::Coordinator;

    let harness = common::Harness::new();
    let registry = registry();
    let detected = detected_from_fixture(a100(&registry), "ubuntu2404-x86-a100-40g-a2hg1");
    let admission =
        PlatformAdmission::evaluate(&registry, &detected, &ObservedStack::default(), None);
    assert!(matches!(admission, PlatformAdmission::Supported { .. }));

    let coordinator = Arc::new(
        Coordinator::new(
            harness.config.clone(),
            harness.store.clone(),
            harness.worker.clone(),
        )
        .with_platform_admission(admission)
        .with_platform_registry(registry),
    );
    let bundle = common::write_bundle(
        harness.td.path(),
        "undeclared-backend",
        common::BundleSpec {
            backend_hint: Some("mock"),
            ..Default::default()
        },
    );

    match coordinator.deploy(
        "undeclared-backend",
        &bundle,
        BTreeMap::default(),
        None,
        None,
    ) {
        Err(AgentError::PlatformNotAdmissible { reason, detail }) => {
            assert_eq!(reason, Some(PlatformReason::MissingBackendPackage));
            assert!(
                detail.contains("mock"),
                "the rejection names the backend: {detail}"
            );
        }
        other => panic!("an undeclared backend must not deploy, got {other:?}"),
    }
    assert!(
        !harness
            .config
            .staging_dir
            .join("undeclared-backend")
            .exists(),
        "backend admission must happen before staging"
    );
    assert!(
        harness.worker.calls().expect("worker calls").is_empty(),
        "backend admission must happen before worker prepare"
    );
}
