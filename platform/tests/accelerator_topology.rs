// SPDX-License-Identifier: Apache-2.0

//! Probe observations must satisfy the complete topology a support row claims.

#![allow(clippy::expect_used)]

use std::path::Path;

use tensorplate_platform::{
    identify_accelerator, AcceleratorSources, DetectedArchitecture, DetectedVendor, ExactHostFacts,
    HostIdentity, HostReport, PlatformReason, PlatformRegistry, PlatformReport, RowMatch,
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

/// The same synthetic row claiming one device, so a ceiling can be compared
/// across counts with nothing else differing.
fn one_l4_registry() -> PlatformRegistry {
    let source = include_str!("../../config/platform/rows/ubuntu2404-x86-l4-g2s8.json");
    let mut document: serde_json::Value = serde_json::from_str(source).expect("committed row");
    document["validation_environment"] = serde_json::json!({
        "kind": "physical", "identity": "synthetic one-device lab server"
    });
    let body = serde_json::to_string(&document).expect("row renders");
    PlatformRegistry::from_documents(
        [(Path::new("one-l4.json"), body.as_str())],
        std::iter::empty(),
    )
    .expect("one-device row loads")
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
        accelerator: Some(detected.observation()),
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

#[test]
fn the_capability_reports_the_count_the_row_claims() {
    // Admission bounds replica count from the resolved capability rather
    // than from the raw observation, so the count has to survive
    // resolution. Read from the row, which is what was validated.
    let two = two_l4_registry();
    let capability = two
        .resolved_capability(&report(&two, &[L4, SECOND_L4]))
        .expect("a matching two-device host resolves a capability");
    assert_eq!(capability.device_count(), 2);

    let one = one_l4_registry();
    let capability = one
        .resolved_capability(&report(&one, &[L4]))
        .expect("a matching one-device host resolves a capability");
    assert_eq!(capability.device_count(), 1);
}

#[test]
fn the_memory_ceiling_is_per_device_and_does_not_scale_with_the_count() {
    // The property that stops a worker being handed a budget no single
    // card can honour. A replica is pinned to one device and cannot reach
    // the others, so eight cards of 24 GiB is a 24 GiB ceiling, not 192.
    //
    // Asserted as an equality between counts rather than against a
    // literal, so it fails on any scaling -- by the count, or by anything
    // else that varies with it.
    let one = one_l4_registry();
    let two = two_l4_registry();
    let single = one
        .resolved_capability(&report(&one, &[L4]))
        .expect("one-device capability");
    let paired = two
        .resolved_capability(&report(&two, &[L4, SECOND_L4]))
        .expect("two-device capability");

    assert_eq!(
        paired.max_resident_model_memory(),
        single.max_resident_model_memory(),
        "the per-device ceiling must not change when the row claims more devices"
    );
    assert_eq!(
        paired.row_memory_budget_bytes(),
        single.row_memory_budget_bytes(),
        "the row budget is per device too"
    );
}

#[test]
fn same_sku_devices_use_the_smaller_capacity_in_either_order() {
    let registry = two_l4_registry();
    let smaller = SECOND_L4.replace("24564", "23034");
    for devices in [[L4, smaller.as_str()], [smaller.as_str(), L4]] {
        let capability = registry
            .resolved_capability(&report(&registry, &devices))
            .expect("same-SKU devices match despite different usable memory");
        assert_eq!(capability.device_count(), 2);
        assert_eq!(
            capability.detected_memory_bytes(),
            Some(23034 * 1024 * 1024)
        );
        assert_eq!(capability.max_resident_model_memory(), 23034 * 1024 * 1024);
    }
}

#[test]
fn an_unreadable_capacity_keeps_the_other_devices_known_bound() {
    let registry = two_l4_registry();
    let unknown = L4.replace("24564", "[N/A]");
    let smaller = SECOND_L4.replace("24564", "23034");
    for devices in [
        [unknown.as_str(), smaller.as_str()],
        [smaller.as_str(), unknown.as_str()],
    ] {
        let capability = registry
            .resolved_capability(&report(&registry, &devices))
            .expect("missing memory does not erase readable identity");
        assert_eq!(capability.device_count(), 2);
        assert_eq!(
            capability.detected_memory_bytes(),
            Some(23034 * 1024 * 1024)
        );
        assert_eq!(capability.max_resident_model_memory(), 23034 * 1024 * 1024);
    }
}

#[test]
fn no_readable_capacities_fall_back_to_the_row_budget() {
    let registry = two_l4_registry();
    let unknown_first = L4.replace("24564", "[N/A]");
    let unknown_second = SECOND_L4.replace("24564", "[N/A]");
    let capability = registry
        .resolved_capability(&report(&registry, &[&unknown_first, &unknown_second]))
        .expect("the validated row still bounds unreadable capacities");
    assert_eq!(capability.device_count(), 2);
    assert_eq!(capability.detected_memory_bytes(), None);
    assert_eq!(
        capability.max_resident_model_memory(),
        capability.row_memory_budget_bytes()
    );
}

#[test]
fn larger_device_readings_do_not_broaden_the_row_budget() {
    let registry = two_l4_registry();
    // Synthetic capacities above the row's nominal budget exercise the
    // other side of the bound after finding the smallest device reading.
    let larger_first = L4.replace("24564", "49152");
    let larger_second = SECOND_L4.replace("24564", "32768");
    let capability = registry
        .resolved_capability(&report(&registry, &[&larger_first, &larger_second]))
        .expect("larger memory does not change SKU matching");
    assert_eq!(capability.device_count(), 2);
    assert_eq!(
        capability.detected_memory_bytes(),
        Some(32768 * 1024 * 1024)
    );
    assert_eq!(
        capability.max_resident_model_memory(),
        capability.row_memory_budget_bytes()
    );
}

#[test]
fn an_unvalidated_environment_keeps_the_same_common_memory_bound() {
    let registry = two_l4_registry();
    let smaller = SECOND_L4.replace("24564", "23034");
    let mut report = report(&registry, &[L4, &smaller]);
    report.host.identity.machine_type = Some("other-shape".to_string());
    let row = registry.rows().next().expect("one row");
    assert_eq!(
        registry.resolve(&report.detected_platform()),
        RowMatch::OutsideValidatedEnvironment {
            candidate: Some(row)
        }
    );
    assert!(registry.resolved_capability(&report).is_none());
    let capability = registry
        .capability_outside_environment(&report, row)
        .expect("prerequisite admission still has a memory bound");
    assert_eq!(capability.device_count(), 2);
    assert_eq!(
        capability.detected_memory_bytes(),
        Some(23034 * 1024 * 1024)
    );
    assert_eq!(capability.max_resident_model_memory(), 23034 * 1024 * 1024);
}
