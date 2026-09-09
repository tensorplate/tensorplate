// SPDX-License-Identifier: Apache-2.0

//! Probe observations must satisfy the complete topology a support row claims.

#![allow(clippy::expect_used)]

use std::path::Path;

use tensorplate_platform::{
    identify_accelerator, AcceleratorObservation, AcceleratorSources, DetectedArchitecture,
    DetectedVendor, ExactHostFacts, HostIdentity, HostReport, PlatformReason, PlatformRegistry,
    PlatformReport, RowMatch,
};

const L4: &str = "NVIDIA L4, 24564, 550.54.15, GPU-00000000-0000-0000-0000-000000000001, [N/A]";
const SECOND_L4: &str =
    "NVIDIA L4, 24564, 550.54.15, GPU-00000000-0000-0000-0000-000000000002, [N/A]";
const A100: &str =
    "NVIDIA A100-SXM4-40GB, 40536, 550.54.15, GPU-00000000-0000-0000-0000-000000000003, Disabled";
const PARTITIONED_A100: &str =
    "NVIDIA A100-SXM4-40GB, 40536, 550.54.15, GPU-00000000-0000-0000-0000-000000000003, Enabled";

fn two_l4_registry() -> PlatformRegistry {
    let source = include_str!("../../config/platform/rows/ubuntu2404-x86-l4-g2s8.json");
    let mut document: serde_json::Value = serde_json::from_str(source).expect("committed row");
    document["accelerator"]["device_count"] = serde_json::json!(2);
    document["validation_environment"] = serde_json::json!({
        "kind": "physical", "identity": "synthetic two-device lab server"
    });
    let body = serde_json::to_string(&document).expect("row renders");
    PlatformRegistry::from_documents(
        [(Path::new("two-l4.json"), body.as_str())],
        std::iter::empty(),
    )
    .expect("two-device row loads")
}

fn report(registry: &PlatformRegistry, devices: &[&str]) -> PlatformReport {
    let row = registry.rows().next().expect("one row");
    let detected = identify_accelerator(&AcceleratorSources {
        nvidia_smi_query: Some(devices.join("\n")),
    })
    .expect("readable devices remain observations")
    .expect("accelerators are present");
    PlatformReport {
        host: HostReport {
            identity: HostIdentity {
                architecture: DetectedArchitecture::Known(row.cpu().architecture),
                vendor: DetectedVendor::Known(row.cpu().vendors[0]),
                os_name: row.os().name.clone(),
                os_version: row.os().version.clone(),
                image_identity: row.os().image_identity.clone(),
                machine_type: row.validation_environment().machine_type.clone(),
            },
            exact: ExactHostFacts::default(),
        },
        accelerator: Some(AcceleratorObservation {
            identity: detected.identity,
            memory_bytes: detected.exact.memory_total_bytes,
            memory_profile: row.accelerator().expect("L4 row").memory_profile,
        }),
    }
}

#[test]
fn homogeneous_devices_match_the_claimed_count() {
    let registry = two_l4_registry();
    let report = report(&registry, &[L4, SECOND_L4]);
    assert!(registry.resolve(&report.detected_platform()).is_supported());
    assert!(registry.resolved_capability(&report).is_some());
}

#[test]
fn mixed_devices_never_inherit_the_first_devices_row_or_capability() {
    let registry = two_l4_registry();
    for devices in [[L4, A100], [A100, L4]] {
        let report = report(&registry, &devices);
        assert_eq!(
            registry.resolve(&report.detected_platform()),
            RowMatch::Unsupported(PlatformReason::UnsupportedAcceleratorTopology),
            "heterogeneous topology must be refused in either order: {devices:?}"
        );
        assert!(registry.resolved_capability(&report).is_none());
    }
}

#[test]
fn partitioning_precedes_a_mixed_topology() {
    let registry = two_l4_registry();
    for devices in [[L4, PARTITIONED_A100], [PARTITIONED_A100, L4]] {
        let report = report(&registry, &devices);
        assert_eq!(
            registry.resolve(&report.detected_platform()),
            RowMatch::Unsupported(PlatformReason::MigModeEnabled)
        );
        assert!(registry.resolved_capability(&report).is_none());
    }
}
