// SPDX-License-Identifier: Apache-2.0
//
// Admission on a machine no row's evidence covers.
//
// The split under test is one the rows already declare. A row that gates
// thermal, power or throttle as load-bearing is one whose cooling belongs
// to the operator, and its evidence does not reach a chassis nobody
// characterised. A row that records those as context is a managed machine,
// and there the missing evidence is about the chassis rather than about
// whether the hardware can serve.
//
// So the same "outside the validated environment" outcome ends two
// different ways, and which one is not a policy written here -- it is read
// off the matched row.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use tensorplate_agent::platform_admission::{ObservedStack, PlatformAdmission};
use tensorplate_platform::{
    identify_accelerator, AcceleratorIdentity, AcceleratorObservation, AcceleratorSources,
    AdmissionPosture, DetectedArchitecture, DetectedVendor, ExactHostFacts, HostIdentity,
    HostReport, PlatformProbeError, PlatformReason, PlatformRegistry, PlatformReport,
    PlatformSupportRow,
};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn registry() -> PlatformRegistry {
    PlatformRegistry::load(&repo_root().join("config/platform")).expect("registry loads")
}

fn row<'a>(registry: &'a PlatformRegistry, id: &str) -> &'a PlatformSupportRow {
    registry
        .row(id)
        .unwrap_or_else(|| panic!("`{id}` is committed"))
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

/// The row's own hardware, so a case only varies the dimension it is about.
fn report_of(row: &PlatformSupportRow, host: HostIdentity) -> PlatformReport {
    let declared = row.accelerator().expect("a row with an accelerator");
    PlatformReport {
        host: HostReport {
            identity: host,
            exact: ExactHostFacts::default(),
        },
        accelerator: Some(AcceleratorObservation {
            identity: AcceleratorIdentity {
                sku: declared.sku.clone(),
                partitioned: false,
            },
            memory_bytes: Some(declared.memory_bytes),
            memory_profile: declared.memory_profile,
        }),
    }
}

/// A host carrying a row's hardware in a chassis that row never validated.
fn in_an_uncharacterised_chassis(row: &PlatformSupportRow, shape: &str) -> PlatformReport {
    let mut host = host_of(row);
    host.machine_type = Some(shape.to_string());
    report_of(row, host)
}

#[test]
fn a_datacenter_row_admits_an_uncharacterised_chassis_on_prerequisites() {
    // The change this feature exists for: bare metal and every non-GCP VM
    // with identical silicon were told `outside_validated_environment`.
    let registry = registry();
    let l4 = row(&registry, "ubuntu2404-x86-l4-g2s8");
    let report = in_an_uncharacterised_chassis(l4, "g2-standard-12");

    let admission =
        PlatformAdmission::evaluate(&registry, &report, &ObservedStack::default(), None);

    assert_eq!(
        admission.validated(),
        Some(false),
        "it runs, but it must not claim evidence recorded on another chassis: {admission:?}"
    );
    assert_eq!(admission.row_id(), Some("ubuntu2404-x86-l4-g2s8"));
    assert_eq!(
        admission.posture(),
        Some((AdmissionPosture::TechnicalPrerequisites, "row floor")),
        "the row's own gate semantics decided this, not a default"
    );
    admission.ensure_supported().expect("it deploys");
}

#[test]
fn an_edge_row_still_requires_the_evidence_to_cover_this_machine() {
    // A Jetson's thermal, power and throttle gates are load-bearing: the
    // chassis is the operator's, and a validation run in someone else's
    // enclosure says nothing about this one.
    // Production, so the support-level check passes it through and the
    // POSTURE gate is what refuses it. `jetson-orin-nx-16gb` is Planned,
    // and using it here tested the support level while appearing to test
    // the chassis gate.
    let registry = registry();
    let jetson = row(&registry, "jetson-orin-nano-8gb-jp62");
    let report = in_an_uncharacterised_chassis(jetson, "a2-highgpu-1g");

    let admission =
        PlatformAdmission::evaluate(&registry, &report, &ObservedStack::default(), None);

    assert_eq!(
        admission.validated(),
        None,
        "an edge row must not be admitted here"
    );
    let err = admission
        .ensure_supported()
        .expect_err("an uncharacterised chassis is refused for an edge row");
    assert!(
        err.to_string().contains("validated environment"),
        "the refusal must say what is actually wrong: {err}"
    );
}

#[test]
fn an_operator_can_pin_strict_and_close_the_prerequisite_path() {
    // The permissive floor is the row's judgement, not a mandate. An
    // operator running a fleet they want uniformly validated can say so.
    let registry = registry();
    let l4 = row(&registry, "ubuntu2404-x86-l4-g2s8");
    let report = in_an_uncharacterised_chassis(l4, "g2-standard-12");

    let admission = PlatformAdmission::evaluate(
        &registry,
        &report,
        &ObservedStack::default(),
        Some(AdmissionPosture::ValidatedRowRequired),
    );

    assert_eq!(
        admission.validated(),
        None,
        "the operator asked for strict: {admission:?}"
    );
    admission
        .ensure_supported()
        .expect_err("pinned strict, so an uncovered chassis is refused");
}

#[test]
fn a_machine_matching_its_row_is_still_admitted_as_validated() {
    // The permissive path must not quietly relabel machines that DO carry
    // evidence -- otherwise nothing is ever reported as validated again.
    let registry = registry();
    let l4 = row(&registry, "ubuntu2404-x86-l4-g2s8");
    let report = report_of(l4, host_of(l4));

    let admission =
        PlatformAdmission::evaluate(&registry, &report, &ObservedStack::default(), None);

    assert_eq!(admission.validated(), Some(true));
    assert_eq!(admission.row_id(), Some("ubuntu2404-x86-l4-g2s8"));
}

#[test]
fn an_unvalidated_admission_is_bounded_by_the_same_memory_ceiling() {
    // "Not validated" must not read as "not bounded". A machine admitted
    // here without a ceiling would be bounded LESS than one that matched.
    let registry = registry();
    let l4 = row(&registry, "ubuntu2404-x86-l4-g2s8");
    let report = in_an_uncharacterised_chassis(l4, "g2-standard-12");

    let admission =
        PlatformAdmission::evaluate(&registry, &report, &ObservedStack::default(), None);
    let capability = admission
        .capability()
        .expect("an unvalidated admission still publishes a ceiling");
    let declared = l4.accelerator().expect("the A100 row has an accelerator");
    assert_eq!(
        capability.max_resident_model_memory(),
        declared.memory_bytes
    );
}

#[test]
fn a_card_the_driver_cannot_answer_for_is_not_an_absent_card() {
    // Without this the host resolves to a CPU-only row and serves on the
    // CPU: `nvidia-smi` needs a working driver to answer, so a broken
    // driver and no card at all produce the same silence. The PCI bus
    // answers without one.
    let registry = registry();
    let cpu_row = row(&registry, "ubuntu2404-x86-cpu");
    let mut report = PlatformReport {
        host: HostReport {
            identity: host_of(cpu_row),
            exact: ExactHostFacts::default(),
        },
        accelerator: None,
    };
    report.host.exact.nvidia_pci_functions = vec!["0000:00:04.0".to_string()];

    let admission =
        PlatformAdmission::evaluate(&registry, &report, &ObservedStack::default(), None);

    assert_eq!(
        admission.reason(),
        Some(PlatformReason::MissingDriverRuntime),
        "a present card with no driver is a driver problem, not an absent card: {admission:?}"
    );
    let err = admission
        .ensure_supported()
        .expect_err("it must not deploy as a CPU box");
    assert!(
        err.to_string().contains("0000:00:04.0"),
        "name the device: {err}"
    );
}

#[test]
fn a_genuinely_cpu_only_host_is_unaffected() {
    // The guard keys on a card being PRESENT. A host with no NVIDIA
    // function on the bus is a CPU box and deploys as one -- otherwise
    // every CPU-only row becomes undeployable.
    let registry = registry();
    let cpu_row = row(&registry, "ubuntu2404-x86-cpu");
    let report = PlatformReport {
        host: HostReport {
            identity: host_of(cpu_row),
            exact: ExactHostFacts::default(),
        },
        accelerator: None,
    };

    let admission =
        PlatformAdmission::evaluate(&registry, &report, &ObservedStack::default(), None);
    assert_eq!(admission.validated(), Some(true), "{admission:?}");
}

#[test]
fn a_working_driver_on_a_gpu_host_is_unaffected() {
    // The same host with the card answering: the PCI functions are still
    // listed, and the guard must not fire on them.
    let registry = registry();
    let l4 = row(&registry, "ubuntu2404-x86-l4-g2s8");
    let mut report = report_of(l4, host_of(l4));
    report.host.exact.nvidia_pci_functions = vec!["0000:00:04.0".to_string()];

    let admission =
        PlatformAdmission::evaluate(&registry, &report, &ObservedStack::default(), None);
    assert_eq!(admission.validated(), Some(true), "{admission:?}");
}

#[test]
fn a_prerequisite_the_row_names_and_the_machine_lacks_is_refused() {
    // Built rather than borrowed: no committed row both records driver
    // components and carries a permissive floor, so the committed set
    // cannot exercise this path at all.
    let registry = registry_with_a_required_component();
    let row = row(&registry, "ubuntu2404-x86-l4-g2s8");
    let report = in_an_uncharacterised_chassis(row, "g2-standard-16");

    let missing = PlatformAdmission::evaluate(&registry, &report, &ObservedStack::default(), None);
    assert_eq!(
        missing.reason(),
        Some(PlatformReason::MissingDriverRuntime),
        "a machine lacking a required component must not be admitted: {missing:?}"
    );

    // Present at ANY version. The row's version is what the evidence run
    // happened to have, not a floor -- refusing a newer driver would
    // refuse most of the fleet this path exists to admit.
    let present = PlatformAdmission::evaluate(
        &registry,
        &report,
        &ObservedStack {
            components: BTreeMap::from([("nvidia_driver".to_string(), "999.99".to_string())]),
            installed_packages: BTreeSet::new(),
        },
        None,
    );
    assert_eq!(
        present.validated(),
        Some(false),
        "a newer driver than the row records must still be admitted: {present:?}"
    );
}

/// The committed L4 row with one driver component added.
fn registry_with_a_required_component() -> PlatformRegistry {
    let body = std::fs::read_to_string(
        repo_root().join("config/platform/rows/ubuntu2404-x86-l4-g2s8.json"),
    )
    .expect("read the L4 row");
    let mut document: serde_json::Value = serde_json::from_str(&body).expect("row parses");
    document["kernel_driver_stack"]["components"] = serde_json::json!([
        {"component": "nvidia_driver", "version": "550.54.15"}
    ]);
    let rendered = serde_json::to_string(&document).expect("row renders");
    PlatformRegistry::from_documents(
        [(std::path::Path::new("l4.json"), rendered.as_str())],
        std::iter::empty(),
    )
    .expect("registry loads")
}

#[test]
fn a_planned_row_is_not_deployable_just_because_the_machine_is_further_from_it() {
    // An exact match on a Planned row is refused as
    // `row_planned_not_validated`. Reaching the outside-environment path
    // means the machine is FURTHER from the row than an exact match --
    // so admitting here what an exact match refuses inverts the gate:
    // the row would become deployable only by running it somewhere its
    // own evidence covers even less.
    //
    // Latent rather than live: every committed Planned row has a
    // load-bearing chassis gate, so the posture check refuses it first.
    // The next Planned server row would not be so lucky.
    let registry = registry_with_l4_at("Planned");
    let l4 = row(&registry, "ubuntu2404-x86-l4-g2s8");
    let report = in_an_uncharacterised_chassis(l4, "g2-standard-16");

    let admission =
        PlatformAdmission::evaluate(&registry, &report, &ObservedStack::default(), None);

    assert_eq!(
        admission.validated(),
        None,
        "a Planned row must not deploy: {admission:?}"
    );
    assert_eq!(
        admission.reason(),
        Some(PlatformReason::RowPlannedNotValidated),
        "and it must say WHY, with the same reason an exact match gives"
    );
}

#[test]
fn an_experimental_row_is_refused_on_the_prerequisite_path_too() {
    let registry = registry_with_l4_at("Experimental");
    let l4 = row(&registry, "ubuntu2404-x86-l4-g2s8");
    let report = in_an_uncharacterised_chassis(l4, "g2-standard-16");

    let admission =
        PlatformAdmission::evaluate(&registry, &report, &ObservedStack::default(), None);

    assert_eq!(
        admission.validated(),
        None,
        "experimental is not deployable: {admission:?}"
    );
    // The frozen vocabulary has no value for experimental, exactly as the
    // exact-match arm records.
    assert_eq!(admission.reason(), None);
    assert!(
        admission
            .ensure_supported()
            .expect_err("refused")
            .to_string()
            .contains("experimental"),
        "the detail must name it"
    );
}

#[test]
fn a_broken_driver_is_named_as_one_even_when_the_probe_itself_fails() {
    // The common broken-driver case is an INSTALLED nvidia-smi exiting
    // non-zero, which is a probe error rather than an absent accelerator.
    // That returns before any matching happens, so the PCI evidence was
    // never consulted and the operator got an untyped detection failure
    // instead of being told their driver is broken.
    let host = HostReport {
        identity: host_of(row(&registry(), "ubuntu2404-x86-cpu")),
        exact: ExactHostFacts {
            nvidia_pci_functions: vec!["0000:00:04.0".to_string()],
            ..ExactHostFacts::default()
        },
    };

    let admission = PlatformAdmission::accelerator_probe_failed(
        &host,
        &PlatformProbeError::Unreadable {
            source_name: "nvidia-smi".to_string(),
            detail: "`nvidia-smi` exited with status 1".to_string(),
        },
    );

    assert_eq!(
        admission.reason(),
        Some(PlatformReason::MissingDriverRuntime),
        "a card on the bus and a probe that cannot answer for it is a driver problem: {admission:?}"
    );
    let rendered = admission
        .ensure_supported()
        .expect_err("refused")
        .to_string();
    assert!(
        rendered.contains("0000:00:04.0"),
        "name the device: {rendered}"
    );
    assert!(
        rendered.contains("exited with status 1"),
        "keep the underlying error: {rendered}"
    );
}

#[test]
fn a_probe_failure_with_no_card_on_the_bus_stays_an_untyped_detection_failure() {
    // Fail closed, but do not claim a driver problem that nothing
    // evidences. Naming `missing_driver_runtime` here would send an
    // operator looking for a driver on a machine that has no card.
    let host = HostReport {
        identity: host_of(row(&registry(), "ubuntu2404-x86-cpu")),
        exact: ExactHostFacts::default(),
    };

    let admission = PlatformAdmission::accelerator_probe_failed(
        &host,
        &PlatformProbeError::Unreadable {
            source_name: "nvidia-smi".to_string(),
            detail: "permission denied".to_string(),
        },
    );

    assert_eq!(
        admission.reason(),
        None,
        "nothing evidences a driver problem: {admission:?}"
    );
    admission
        .ensure_supported()
        .expect_err("still fails closed");
}

/// The committed L4 row at a chosen support level. No committed row is
/// both permissively gated and Planned, so the case has to be built.
fn registry_with_l4_at(support_level: &str) -> PlatformRegistry {
    let body = std::fs::read_to_string(
        repo_root().join("config/platform/rows/ubuntu2404-x86-l4-g2s8.json"),
    )
    .expect("read the L4 row");
    let mut document: serde_json::Value = serde_json::from_str(&body).expect("row parses");
    document["support_level"] = serde_json::json!(support_level);
    // The loader refuses a Planned or Experimental row that still claims
    // evidence, and the key is ABSENT on such a row rather than null --
    // both rules cost this fixture a round trip.
    let fields = document.as_object_mut().expect("a row is an object");
    fields.remove("evidence");
    if support_level == "Planned" {
        // And no model-class claims either, until it is validated --
        // required as a field, so emptied rather than removed.
        fields.insert("model_class_rows".to_string(), serde_json::json!([]));
    }
    let rendered = serde_json::to_string(&document).expect("row renders");
    PlatformRegistry::from_documents(
        [(std::path::Path::new("l4.json"), rendered.as_str())],
        std::iter::empty(),
    )
    .expect("registry loads")
}

#[test]
fn a_working_driver_reporting_a_topology_we_cannot_serve_is_not_a_driver_fault() {
    // `nvidia-smi` ANSWERED here -- the driver is fine. The answer is more
    // than one GPU, which no row in this release claims, so detection
    // refuses to interpret it. Blaming `missing_driver_runtime` for that
    // sends an operator to reinstall a driver that is working.
    //
    // Driven through the real probe rather than a hand-built error, so the
    // test breaks if multi-GPU stops producing `Unrecognized`.
    let two_cards = "NVIDIA L4, 23034, 550.54.15, GPU-1111, Disabled\n                     NVIDIA L4, 23034, 550.54.15, GPU-2222, Disabled";
    let error = identify_accelerator(&AcceleratorSources {
        nvidia_smi_query: Some(two_cards.to_string()),
    })
    .expect_err("two GPUs cannot be interpreted as one device");
    assert!(
        matches!(error, PlatformProbeError::Unrecognized { .. }),
        "a readable-but-uninterpretable answer is Unrecognized, got {error:?}"
    );

    let host = HostReport {
        identity: host_of(row(&registry(), "ubuntu2404-x86-cpu")),
        exact: ExactHostFacts {
            // The cards ARE on the bus. Under the untyped-by-PCI-alone
            // rule this is exactly the case that got misblamed.
            nvidia_pci_functions: vec!["0000:00:04.0".to_string(), "0000:00:05.0".to_string()],
            ..ExactHostFacts::default()
        },
    };

    let admission = PlatformAdmission::accelerator_probe_failed(&host, &error);

    assert_eq!(
        admission.reason(),
        None,
        "the driver answered; this is an unsupported topology, not a driver fault: {admission:?}"
    );
    admission
        .ensure_supported()
        .expect_err("still fails closed");
}
