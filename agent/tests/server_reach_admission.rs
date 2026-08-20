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
    AcceleratorIdentity, AcceleratorObservation, AdmissionPosture, DetectedArchitecture,
    DetectedVendor, ExactHostFacts, HostIdentity, HostReport, PlatformReason, PlatformRegistry,
    PlatformReport, PlatformSupportRow,
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
    let a100 = row(&registry, "ubuntu2404-x86-a100-40g-a2hg1");
    let report = in_an_uncharacterised_chassis(a100, "a2-ultragpu-1g");

    let admission =
        PlatformAdmission::evaluate(&registry, &report, &ObservedStack::default(), None);

    assert_eq!(
        admission.validated(),
        Some(false),
        "it runs, but it must not claim evidence recorded on another chassis: {admission:?}"
    );
    assert_eq!(admission.row_id(), Some("ubuntu2404-x86-a100-40g-a2hg1"));
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
    let registry = registry();
    let jetson = row(&registry, "jetson-orin-nx-16gb");
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
    let a100 = row(&registry, "ubuntu2404-x86-a100-40g-a2hg1");
    let report = in_an_uncharacterised_chassis(a100, "a2-ultragpu-1g");

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
    let a100 = row(&registry, "ubuntu2404-x86-a100-40g-a2hg1");
    let report = report_of(a100, host_of(a100));

    let admission =
        PlatformAdmission::evaluate(&registry, &report, &ObservedStack::default(), None);

    assert_eq!(admission.validated(), Some(true));
    assert_eq!(admission.row_id(), Some("ubuntu2404-x86-a100-40g-a2hg1"));
}

#[test]
fn an_unvalidated_admission_is_bounded_by_the_same_memory_ceiling() {
    // "Not validated" must not read as "not bounded". A machine admitted
    // here without a ceiling would be bounded LESS than one that matched.
    let registry = registry();
    let a100 = row(&registry, "ubuntu2404-x86-a100-40g-a2hg1");
    let report = in_an_uncharacterised_chassis(a100, "a2-ultragpu-1g");

    let admission =
        PlatformAdmission::evaluate(&registry, &report, &ObservedStack::default(), None);
    let capability = admission
        .capability()
        .expect("an unvalidated admission still publishes a ceiling");
    let declared = a100.accelerator().expect("the A100 row has an accelerator");
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
    let a100 = row(&registry, "ubuntu2404-x86-a100-40g-a2hg1");
    let mut report = report_of(a100, host_of(a100));
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
