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

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;

use tensorplate_agent::error::AgentError;
use tensorplate_agent::platform_admission::{
    check_backend_packages, ObservedStack, PlatformAdmission,
};
use tensorplate_platform::{
    identify, identify_accelerator, identify_jetson_accelerator, identify_platform,
    AcceleratorIdentity, AcceleratorObservation, AcceleratorSources, AdmissionPosture,
    DetectedArchitecture, DetectedVendor, ExactHostFacts, HostIdentity, HostReport, HostSources,
    PlatformReason, PlatformRegistry, PlatformReport, PlatformSupportRow, SignalName,
    SignalOutcome,
};
use tensorplate_protocol::{
    AgentRunState, PlatformSignalOutcomeStatus, PlatformTelemetryGate, PlatformTelemetrySignalName,
};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn registry() -> PlatformRegistry {
    PlatformRegistry::load(&repo_root().join("config/platform")).expect("registry loads")
}

/// The A100 row, which is the one the MIG fixtures are written against.
/// Planned since the A100 quota was refused, so it anchors only the
/// partitioning checks, which run before support level is considered.
fn a100(registry: &PlatformRegistry) -> &PlatformSupportRow {
    registry
        .row("ubuntu2404-x86-a100-40g-a2hg1")
        .expect("the A100 row is committed")
}

/// The L4 row: the Production datacenter exemplar for everything that
/// needs a supported row with an accelerator fixture behind it.
fn l4(registry: &PlatformRegistry) -> &PlatformSupportRow {
    registry
        .row("ubuntu2404-x86-l4-g2s8")
        .expect("the L4 row is committed")
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

/// The canonical L4 observation with the fixture's usable framebuffer,
/// rather than substituting the row's nominal capacity.
fn observed_l4_report(registry: &PlatformRegistry) -> PlatformReport {
    let row = l4(registry);
    let text = std::fs::read_to_string(
        repo_root().join("test/platform/accelerator/ubuntu2404-x86-l4-g2s8.txt"),
    )
    .expect("read L4 fixture");
    let card = identify_accelerator(&AcceleratorSources {
        nvidia_smi_query: Some(text),
    })
    .expect("fixture parses")
    .expect("fixture reports one card");
    PlatformReport {
        host: HostReport {
            identity: host_of(row),
            exact: ExactHostFacts {
                host_total_memory_bytes: Some(32 * 1024 * 1024 * 1024),
                ..ExactHostFacts::default()
            },
        },
        accelerator: Some(AcceleratorObservation {
            identity: card.identity,
            memory_bytes: card.exact.memory_total_bytes,
            memory_profile: row.accelerator().expect("accelerator row").memory_profile,
        }),
    }
}

fn all_signal_outcomes() -> BTreeMap<SignalName, SignalOutcome> {
    SignalName::all()
        .into_iter()
        .map(|name| (name, SignalOutcome::Collected))
        .collect()
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
fn the_same_card_unpartitioned_is_not_refused_for_partitioning() {
    // The control for the case above. Without it, a check that refused
    // everything would look correct. The A100 row is Planned, so the
    // unpartitioned card is refused too -- but for that reason and not
    // this one, which is what the control has to show.
    let registry = registry();
    let detected = detected_from_fixture(a100(&registry), "ubuntu2404-x86-a100-40g-a2hg1");
    match PlatformAdmission::evaluate(&registry, &detected, &ObservedStack::default(), None) {
        PlatformAdmission::Rejected {
            row_id,
            reason: Some(reason),
            ..
        } => {
            assert_eq!(row_id.as_deref(), Some("ubuntu2404-x86-a100-40g-a2hg1"));
            assert_eq!(reason, PlatformReason::RowPlannedNotValidated);
        }
        other => panic!("expected the Planned refusal, got {other:?}"),
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
                device_count: 1,
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
    let row = l4(&registry);
    let detected = detected_from_fixture(row, "ubuntu2404-x86-l4-g2s8");

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
    let detected = detected_from_fixture(l4(&staged), "ubuntu2404-x86-l4-g2s8");

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
    let row_path = staging.join("rows/ubuntu2404-x86-l4-g2s8.json");
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
    let row = l4(&registry);
    let mut host = host_of(row);
    host.machine_type = Some("g2-standard-12".to_string());
    let report = identify_accelerator(&AcceleratorSources {
        nvidia_smi_query: Some(
            std::fs::read_to_string(
                repo_root().join("test/platform/accelerator/ubuntu2404-x86-l4-g2s8.txt"),
            )
            .expect("read fixture"),
        ),
    })
    .expect("detection succeeds")
    .expect("one device");
    let detected = report_of(row, host, Some(report.identity));

    // Pinned strict by the operator. Without this the A100 is a
    // datacenter row, whose floor now admits an uncharacterised chassis on
    // prerequisites -- the subject here is the SHAPE of a refusal, so the
    // case has to be one that still refuses.
    match PlatformAdmission::evaluate(
        &registry,
        &detected,
        &ObservedStack::default(),
        Some(AdmissionPosture::ValidatedRowRequired),
    ) {
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
    let detected = detected_from_fixture(l4(&registry), "ubuntu2404-x86-l4-g2s8");
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

/// The recorded sources from the in-lab Orin Nano, read from the fixture
/// rather than restated here so this test and detection's own fixture
/// tests cannot drift apart.
fn host_fixture_sources(name: &str) -> HostSources {
    let body = std::fs::read_to_string(
        repo_root().join(format!("test/platform/host_identity/{name}.json")),
    )
    .expect("read host fixture");
    let fixture: serde_json::Value = serde_json::from_str(&body).expect("fixture parses");
    let text = |key: &str| {
        fixture["sources"]
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    HostSources {
        uname_machine: text("uname_machine"),
        os_release: text("os_release"),
        cpuinfo: text("cpuinfo"),
        nv_tegra_release: text("nv_tegra_release"),
        nvidia_jetpack_version: text("nvidia_jetpack_version"),
        device_tree_model: text("device_tree_model"),
        sw_vers_product_name: text("sw_vers_product_name"),
        sw_vers_product_version: text("sw_vers_product_version"),
        sw_vers_build_version: text("sw_vers_build_version"),
        cpu_brand: text("cpu_brand"),
        hw_memsize: text("hw_memsize"),
        gce_machine_type: text("gce_machine_type"),
        proc_meminfo: text("proc_meminfo"),
        pci_devices: text("pci_devices"),
    }
}

fn lab_jetson_sources() -> HostSources {
    host_fixture_sources("lab-jetson-orin-nano-l4t-r36.5")
}

#[test]
fn the_lab_jetson_is_admitted_on_the_stack_the_agent_observes() {
    // Detection resolving the row is not the same as the agent admitting
    // it. `observe_platform` reports no stack components, so a row that
    // records one is measured against a stack that never contains it --
    // which rejects the row on the very machine it was validated on.
    // Driven from the recorded sources so the whole path is under test,
    // and with the empty stack production actually supplies.
    let registry = registry();
    let row = registry
        .row("jetson-orin-nano-8gb-jp62")
        .expect("the Orin Nano row is committed");
    let sources = lab_jetson_sources();
    let identity = identify(&sources).expect("detection succeeds").identity;
    let accelerator =
        identify_jetson_accelerator(&sources).expect("a Jetson yields an accelerator identity");
    let report = report_of(row, identity, Some(accelerator));

    match PlatformAdmission::evaluate(&registry, &report, &ObservedStack::default(), None) {
        PlatformAdmission::Supported {
            row_id, validated, ..
        } => {
            assert_eq!(row_id, "jetson-orin-nano-8gb-jp62");
            assert!(validated, "the lab device is this row's own evidence");
        }
        PlatformAdmission::Rejected { reason, detail, .. } => {
            panic!("the lab Jetson must be admitted at startup, got Rejected({reason:?}): {detail}")
        }
    }
}

#[test]
fn a_jetpack_6_1_jetson_is_not_admitted_to_the_6_2_row() {
    // L4T 36.4 with no `nvidia-jetpack` package to read a version from is
    // JetPack 6.1, not 6.2 -- NVIDIA's archive pairs base 36.4 with 6.1 and
    // only 36.4.3 onward with 6.2.x. The board is identical, so nothing but
    // the L4T revision distinguishes it from the row's own device, and
    // admitting it would serve a Production claim on a platform whose
    // evidence was never collected.
    let mut sources = lab_jetson_sources();
    sources.nv_tegra_release = Some(
        "# R36 (release), REVISION: 4.0, GCID: 41000000, BOARD: generic, EABI: aarch64, DATE: Thu Jan 15 19:24:38 UTC 2026\n"
            .to_string(),
    );
    let registry = registry();
    let row = registry
        .row("jetson-orin-nano-8gb-jp62")
        .expect("the Orin Nano row is committed");
    let identity = identify(&sources).expect("detection succeeds").identity;
    let accelerator =
        identify_jetson_accelerator(&sources).expect("a Jetson yields an accelerator identity");
    let report = report_of(row, identity, Some(accelerator));

    match PlatformAdmission::evaluate(&registry, &report, &ObservedStack::default(), None) {
        PlatformAdmission::Supported { row_id, .. } => panic!(
            "a JetPack 6.1 device must not be admitted to the 6.2 row, got Supported({row_id})"
        ),
        PlatformAdmission::Rejected { .. } => {}
    }
}

#[test]
fn startup_memory_reaches_status_as_a_usable_bounded_ceiling() {
    use std::sync::Arc;
    use tensorplate_agent::coordinator::Coordinator;

    let registry = registry();
    let l4_report = observed_l4_report(&registry);
    let l4_observed = l4_report
        .accelerator
        .as_ref()
        .and_then(|accelerator| accelerator.memory_bytes)
        .expect("recorded L4 fixture carries usable framebuffer");
    let l4_admission =
        PlatformAdmission::evaluate(&registry, &l4_report, &ObservedStack::default(), None);
    let harness = common::Harness::new();
    let coordinator = Arc::new(
        Coordinator::new(
            harness.config.clone(),
            harness.store.clone(),
            harness.worker.clone(),
        )
        .with_platform_admission(l4_admission),
    );
    let status = coordinator.status().expect("status");
    assert_eq!(status.agent_state, AgentRunState::Ready);
    let projected = status
        .platform_telemetry
        .expect("startup admission projects telemetry");
    assert!(
        projected.signals.is_empty(),
        "no live signal snapshot was supplied"
    );
    let wire = serde_json::to_value(&projected).expect("serialize memory-only telemetry");
    assert!(
        wire.get("signals").is_none(),
        "no collector snapshot is omitted on the wire rather than encoded as an empty snapshot"
    );
    let memory = projected.memory.expect("accelerator row projects memory");
    assert!(l4_observed < memory.row_nominal_capacity_bytes);
    assert_eq!(memory.effective_budget_bytes, Some(l4_observed));

    let jetson_report = identify_platform(&lab_jetson_sources()).expect("Jetson fixture detects");
    let jetson_observed = jetson_report
        .accelerator
        .as_ref()
        .and_then(|accelerator| accelerator.memory_bytes)
        .expect("Jetson fixture carries usable shared memory");
    let jetson =
        PlatformAdmission::evaluate(&registry, &jetson_report, &ObservedStack::default(), None)
            .telemetry_status()
            .expect("Jetson is supported");
    let memory = jetson.memory.expect("Jetson projects shared memory");
    assert!(memory.shares_one_pool);
    assert!(jetson_observed < memory.row_nominal_capacity_bytes);
    assert_eq!(memory.effective_budget_bytes, Some(jetson_observed));
}

#[test]
fn a_context_only_failure_degrades_status_without_rejecting_deployment() {
    use tensorplate_agent::coordinator::Coordinator;

    let registry = registry();
    let report = observed_l4_report(&registry);
    let mut outcomes = all_signal_outcomes();
    outcomes.insert(
        SignalName::Thermal,
        SignalOutcome::Unavailable {
            detail: "temperature source timed out".into(),
        },
    );
    let admission = PlatformAdmission::evaluate_with_signal_outcomes(
        &registry,
        &report,
        &ObservedStack::default(),
        None,
        &outcomes,
    );
    admission
        .ensure_supported()
        .expect("the L4 thermal gate is context-only");

    let harness = common::Harness::new();
    let status = Coordinator::new(
        harness.config.clone(),
        harness.store.clone(),
        harness.worker.clone(),
    )
    .with_platform_admission(admission)
    .status()
    .expect("status");
    assert_eq!(status.agent_state, AgentRunState::Degraded);
    let telemetry = status.platform_telemetry.expect("telemetry projected");
    assert_eq!(
        telemetry.degraded_reason.as_deref(),
        Some("telemetry_degraded")
    );
    assert!(!telemetry.deployment_degraded);
    assert_eq!(telemetry.signals.len(), 5);
    let names: HashSet<_> = telemetry.signals.iter().map(|signal| signal.name).collect();
    assert_eq!(
        names.len(),
        5,
        "each stable signal name appears exactly once"
    );
    assert!(names.contains(&PlatformTelemetrySignalName::Thermal));
    assert!(telemetry.signals.iter().all(|signal| {
        signal.outcome.is_none() == (signal.gate == PlatformTelemetryGate::NotApplicable)
    }));
}

#[test]
fn observed_memory_completes_a_four_non_memory_signal_snapshot() {
    let registry = registry();
    let jetson = identify_platform(&lab_jetson_sources()).expect("Jetson fixture detects");
    let l4 = observed_l4_report(&registry);
    let mut non_memory = all_signal_outcomes();
    non_memory.remove(&SignalName::Memory);

    for (name, report) in [("L4", l4), ("Jetson", jetson)] {
        let admission = PlatformAdmission::evaluate_with_signal_outcomes(
            &registry,
            &report,
            &ObservedStack::default(),
            None,
            &non_memory,
        );
        admission.ensure_supported().unwrap_or_else(|err| {
            panic!("{name}: observed memory must complete the snapshot: {err}")
        });
        let telemetry = admission
            .telemetry_status()
            .expect("supported row projects");
        assert_eq!(telemetry.degraded_reason, None, "{name}");
        let memory = telemetry
            .signals
            .iter()
            .find(|signal| signal.name == PlatformTelemetrySignalName::Memory)
            .unwrap_or_else(|| panic!("{name}: memory signal projected"));
        assert_eq!(
            memory.outcome,
            Some(PlatformSignalOutcomeStatus::Collected),
            "{name}: startup memory observation supplies the memory collector outcome"
        );
    }

    let mut explicit_failure = non_memory;
    explicit_failure.insert(
        SignalName::Memory,
        SignalOutcome::Unavailable {
            detail: "memory collector explicitly failed".into(),
        },
    );
    let report = observed_l4_report(&registry);
    let admission = PlatformAdmission::evaluate_with_signal_outcomes(
        &registry,
        &report,
        &ObservedStack::default(),
        None,
        &explicit_failure,
    );
    assert!(
        matches!(
            admission.ensure_supported(),
            Err(AgentError::PlatformNotAdmissible {
                reason: Some(PlatformReason::TelemetryDegraded),
                ..
            })
        ),
        "an explicit memory collector outcome must override the startup observation"
    );
}

#[test]
fn omitted_load_bearing_outcome_blocks_deploy_and_is_explicit_in_status() {
    use std::sync::Arc;
    use tensorplate_agent::coordinator::Coordinator;

    let registry = registry();
    let report = identify_platform(&lab_jetson_sources()).expect("Jetson fixture detects");
    let mut outcomes = all_signal_outcomes();
    outcomes.remove(&SignalName::Thermal);
    outcomes.insert(
        SignalName::GpuUtilization,
        SignalOutcome::Unavailable {
            detail: "utilization source timed out".into(),
        },
    );
    let admission = PlatformAdmission::evaluate_with_signal_outcomes(
        &registry,
        &report,
        &ObservedStack::default(),
        None,
        &outcomes,
    );
    match admission.ensure_supported() {
        Err(AgentError::PlatformNotAdmissible { reason, detail }) => {
            assert_eq!(reason, Some(PlatformReason::TelemetryDegraded));
            assert!(detail.contains("thermal"));
            assert!(
                !detail.contains("gpu_utilization"),
                "context-only failures stay in status but are not named as deployment blockers: \
                 {detail}"
            );
        }
        other => panic!("omitted Jetson thermal outcome must reject, got {other:?}"),
    }

    let harness = common::Harness::new();
    let coordinator = Arc::new(
        Coordinator::new(
            harness.config.clone(),
            harness.store.clone(),
            harness.worker.clone(),
        )
        .with_platform_admission(admission)
        .with_platform_registry(registry),
    );
    let status = coordinator.status().expect("status");
    assert_eq!(status.agent_state, AgentRunState::Degraded);
    let telemetry = status.platform_telemetry.expect("telemetry projected");
    assert_eq!(telemetry.signals.len(), 5);
    assert!(telemetry.deployment_degraded);
    let names: HashSet<_> = telemetry.signals.iter().map(|signal| signal.name).collect();
    assert_eq!(names.len(), 5, "no signal may be duplicated or omitted");
    let thermal = telemetry
        .signals
        .iter()
        .find(|signal| signal.name == PlatformTelemetrySignalName::Thermal)
        .expect("thermal projected");
    assert!(matches!(
        thermal.outcome,
        Some(PlatformSignalOutcomeStatus::Unavailable { .. })
    ));

    let bundle = common::write_bundle(
        harness.td.path(),
        "telemetry-degraded",
        common::BundleSpec::default(),
    );
    match coordinator.deploy("telemetry-degraded", &bundle, BTreeMap::new(), None, None) {
        Err(AgentError::PlatformNotAdmissible { reason, .. }) => {
            assert_eq!(reason, Some(PlatformReason::TelemetryDegraded));
        }
        other => panic!("load-bearing telemetry must block deploy, got {other:?}"),
    }
}

#[test]
fn generated_projection_uses_no_outcome_only_for_not_applicable_signals() {
    let registry = registry();
    let report = identify_platform(&host_fixture_sources("macos26-m1pro-16gb"))
        .expect("Mac fixture detects");
    let telemetry = PlatformAdmission::evaluate_with_signal_outcomes(
        &registry,
        &report,
        &ObservedStack::default(),
        None,
        &all_signal_outcomes(),
    )
    .telemetry_status()
    .expect("Mac row supported");
    assert_eq!(telemetry.signals.len(), 5);
    assert!(telemetry.signals.iter().all(|signal| {
        signal.outcome.is_none() == (signal.gate == PlatformTelemetryGate::NotApplicable)
    }));
    let absent: HashSet<_> = telemetry
        .signals
        .iter()
        .filter(|signal| signal.outcome.is_none())
        .map(|signal| signal.name)
        .collect();
    assert_eq!(
        absent,
        HashSet::from([
            PlatformTelemetrySignalName::Power,
            PlatformTelemetrySignalName::GpuUtilization,
        ])
    );

    let projected = serde_json::to_value(&telemetry).expect("serialize projection");
    for signal in projected["signals"].as_array().expect("signal array") {
        assert_eq!(
            signal.get("outcome").is_none(),
            signal["gate"] == "not_applicable",
            "only not-applicable signals omit an outcome: {signal}"
        );
    }
    let envelope = serde_json::json!({
        "schema_version": tensorplate_protocol::SCHEMA_VERSION,
        "status": "ok",
        "agent_status": {
            "agent_state": "ready",
            "platform_telemetry": projected,
        }
    });
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../../protocol/schemas/agent_control.json"))
            .expect("control schema parses");
    let validator = jsonschema::JSONSchema::compile(&schema).expect("control schema compiles");
    assert!(
        validator.is_valid(&envelope),
        "generated projection must satisfy the public schema: {envelope}"
    );
}
