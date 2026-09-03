// SPDX-License-Identifier: Apache-2.0
//
// The unsupported-combination matrix: one table, one row per way a
// machine can fail to be a supported combination, each asserting the
// specific typed reason rather than merely that it failed.
//
// Scattered tests already cover most of these conditions individually.
// What a table buys is different: the reasons are a fixed vocabulary, and
// a table makes it visible when one of them has no case at all — which is
// how two of the ten came to have no producer. Each case names the
// dimension it is off-matrix in, so a wrong-but-plausible reason (a bad
// SKU reported as a bad OS) fails here rather than reaching an operator.
//
// The agent is the layer where every case is observable: it resolves
// through the registry and owns backend admission.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;

use serde_json::Value;
use tensorplate_agent::platform_admission::{check_backend_packages, ObservedStack};
use tensorplate_agent::PlatformAdmission;
use tensorplate_platform::{
    identify_accelerator, identify_platform, AcceleratorObservation, AcceleratorSources,
    HostSources, PlatformReason, PlatformRegistry, PlatformReport, SignalName, SignalOutcome,
    SignalTelemetry,
};
use tensorplate_protocol::backend_probe::BackendProbeState;
use tensorplate_protocol::PlatformMemoryProfileName;

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

fn registry() -> PlatformRegistry {
    PlatformRegistry::load(&repo_path("config/platform")).expect("registry loads")
}

fn sources(name: &str) -> HostSources {
    let path = repo_path(&format!("test/platform/host_identity/{name}.json"));
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let fixture: Value = serde_json::from_str(&body).expect("fixture parses");
    let raw = &fixture["sources"];
    let text = |key: &str| raw.get(key).and_then(Value::as_str).map(str::to_string);
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

/// Detect a host fixture, optionally pairing it with a recorded
/// accelerator answer the way a real discrete-GPU host reports one.
fn report(host: &str, accelerator: Option<&str>) -> PlatformReport {
    let mut report = identify_platform(&sources(host)).expect("fixture detects");
    if let Some(name) = accelerator {
        let raw =
            std::fs::read_to_string(repo_path(&format!("test/platform/accelerator/{name}.txt")))
                .unwrap_or_else(|err| panic!("read accelerator fixture {name}: {err}"));
        let card = identify_accelerator(&AcceleratorSources {
            nvidia_smi_query: Some(raw),
        })
        .expect("accelerator fixture interprets")
        .expect("accelerator fixture carries a device");
        report.accelerator = Some(AcceleratorObservation {
            identity: card.identity,
            memory_bytes: card.exact.memory_total_bytes,
            memory_profile: PlatformMemoryProfileName::DiscreteGpu,
        });
    }
    report
}

/// The reason admission reports for a machine, or `None` when it admits.
fn admission_reason(report: &PlatformReport) -> Option<PlatformReason> {
    match PlatformAdmission::evaluate(&registry(), report, &ObservedStack::default(), None) {
        PlatformAdmission::Rejected { reason, .. } => reason,
        PlatformAdmission::Supported { .. } => None,
    }
}

/// One row of the matrix: what is wrong, what admission reported, and the
/// one reason that names the dimension it is off-matrix in.
struct Case {
    what: &'static str,
    got: Option<PlatformReason>,
    want: PlatformReason,
}

/// Cases resolvable from platform identity alone.
fn identity_cases() -> Vec<Case> {
    let mut old_macos = sources("macos26-m1pro-16gb");
    old_macos.sw_vers_product_version = Some("15.6.1".to_string());

    let mut riscv = sources("ubuntu2404-x86-cpu");
    riscv.uname_machine = Some("riscv64".to_string());

    let mut unknown_vendor = sources("ubuntu2404-x86-cpu");
    unknown_vendor.cpuinfo = Some("vendor_id\t: CentaurHauls\nmodel name\t: VIA C7\n".to_string());

    vec![
        // The accelerator is off-matrix on an otherwise-exact host.
        Case {
            what: "unsupported accelerator SKU",
            got: admission_reason(&report(
                "ubuntu2404-x86-l4-g2s8",
                Some("unsupported-rtx-a6000"),
            )),
            want: PlatformReason::UnsupportedAcceleratorSku,
        },
        Case {
            what: "macOS below the row floor",
            got: admission_reason(&identify_platform(&old_macos).expect("detects")),
            want: PlatformReason::UnsupportedOsVersion,
        },
        // The OS is fine; the silicon is not.
        Case {
            what: "non-M-series Apple chip",
            got: admission_reason(&report("macos26-apple-a17pro-unsupported", None)),
            want: PlatformReason::UnsupportedAcceleratorSku,
        },
        Case {
            what: "exact Planned row",
            got: admission_reason(&report("jetson-orin-nx-16gb", None)),
            want: PlatformReason::RowPlannedNotValidated,
        },
        Case {
            what: "unsupported CPU architecture",
            got: admission_reason(&identify_platform(&riscv).expect("detects")),
            want: PlatformReason::UnsupportedCpuArch,
        },
        Case {
            what: "unsupported CPU vendor",
            got: admission_reason(&identify_platform(&unknown_vendor).expect("detects")),
            want: PlatformReason::UnsupportedCpuVendor,
        },
        // A supported card, partitioned. Refused for the partitioning and
        // not for its identity, which is why that check precedes the SKU.
        Case {
            what: "MIG-enabled accelerator",
            got: admission_reason(&report(
                "ubuntu2404-x86-a100-40g-a2hg1",
                Some("mig-enabled-a100-40g"),
            )),
            want: PlatformReason::MigModeEnabled,
        },
        // Deliberately NOT the OS reason: 22.04 is supported, and telling
        // this operator their OS is unsupported would send them to
        // reinstall a platform that is fine. What no row covers is this
        // accelerator here.
        Case {
            what: "GPU on an OS release with no GPU row",
            got: admission_reason(&report(
                "ubuntu2204-x86-cpu",
                Some("ubuntu2404-x86-l4-g2s8"),
            )),
            want: PlatformReason::UnsupportedAcceleratorSku,
        },
        // An accelerator the PCI bus sees and no driver could identify.
        Case {
            what: "driver absent on a GPU host",
            got: admission_reason(&report("ubuntu2404-x86-l4-g2s8", None)),
            want: PlatformReason::MissingDriverRuntime,
        },
    ]
}

/// Cases that need the backend layer rather than platform identity.
fn backend_cases() -> Vec<Case> {
    let registry = registry();
    let l4 = registry
        .row("ubuntu2404-x86-l4-g2s8")
        .expect("the L4 row is committed");
    // Read through the error record, which is what a CLI and the durable
    // store see -- a reason that stops at the crate boundary is not one
    // an operator can act on.
    let missing_package = check_backend_packages(l4, "python_pytorch", &BTreeSet::new())
        .expect_err("an uninstalled package must refuse")
        .to_record()
        .context
        .and_then(|c| PlatformReason::try_from(c).ok());

    // Drive the real producer: a resolved row plus the complete collector
    // snapshot with one unavailable applicable signal.
    let mut outcomes: BTreeMap<_, _> = SignalName::all()
        .into_iter()
        .map(|name| (name, SignalOutcome::Collected))
        .collect();
    outcomes.insert(
        SignalName::Thermal,
        SignalOutcome::Unavailable {
            detail: "sensor read failed".into(),
        },
    );
    let telemetry_reason = SignalTelemetry::resolve(l4, &outcomes).degraded_reason();

    vec![
        Case {
            what: "missing backend package",
            got: missing_package,
            want: PlatformReason::MissingBackendPackage,
        },
        // The package IS installed and its runtime will not run -- the
        // unavailable-runtime case the vocabulary keeps distinct.
        Case {
            what: "accelerator runtime installed but unusable",
            got: PlatformReason::for_backend_probe(&BackendProbeState::PytorchMissing {
                detail: "No module named 'torch'".into(),
            }),
            want: PlatformReason::AcceleratorRuntimeUnavailable,
        },
        Case {
            what: "telemetry collector failure",
            got: telemetry_reason,
            want: PlatformReason::TelemetryDegraded,
        },
    ]
}

fn all_cases() -> Vec<Case> {
    let mut cases = identity_cases();
    cases.extend(backend_cases());
    cases
}

#[test]
fn every_off_matrix_combination_reports_its_own_dimension() {
    for case in all_cases() {
        assert_eq!(
            case.got,
            Some(case.want),
            "{}: must report the dimension it is actually off-matrix in",
            case.what
        );
    }
}

#[test]
fn the_matrix_covers_every_reason_the_vocabulary_owns() {
    // The property a table exists for. Two of the ten reasons reached
    // this release with no producer at all; a coverage assertion is what
    // would have said so. `telemetry_degraded` is driven through the real
    // row-aware resolver above rather than manufactured by this matrix.
    let covered: HashSet<PlatformReason> = all_cases().into_iter().map(|c| c.want).collect();
    for reason in PlatformReason::ALL {
        assert!(
            covered.contains(&reason),
            "`{}` has no case in the matrix",
            reason.as_str()
        );
    }
}

#[test]
fn a_canonical_m_series_chip_resolves_rather_than_refusing() {
    // The control the off-matrix cases need. Without it a matrix that
    // refused everything would look complete: this asserts the Apple
    // family row still admits the chip it exists for.
    assert_eq!(
        admission_reason(&report("macos26-m2pro-16gb", None)),
        None,
        "a canonical M-series chip must resolve to the family Preview row"
    );
}
