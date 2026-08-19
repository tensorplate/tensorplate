// SPDX-License-Identifier: Apache-2.0
//
// Apple silicon accelerator identity and unified-memory capability fixtures.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use serde_json::Value;
use tensorplate_platform::{
    identify_platform, HostSources, PlatformProbeError, PlatformReason, PlatformRegistry, RowMatch,
};
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

#[test]
fn m1_pro_resolves_to_the_production_row_and_publishes_unified_memory() {
    let report =
        identify_platform(&sources("macos26-m1pro-16gb")).expect("M1 Pro detection succeeds");
    let observed = report.accelerator.as_ref().expect("Apple accelerator");
    assert_eq!(observed.identity.sku, "Apple M1 Pro");
    assert_eq!(observed.memory_bytes, Some(17_179_869_184));
    assert_eq!(
        observed.memory_profile,
        PlatformMemoryProfileName::UnifiedMemory
    );

    let registry = registry();
    let detected = report.detected_platform();
    let RowMatch::Supported(row) = registry.resolve(&detected) else {
        panic!("M1 Pro must resolve to its supported row");
    };
    assert_eq!(row.row_id(), "macos26-m1pro-16gb");

    let capability = registry
        .resolved_capability(&report)
        .expect("supported accelerator publishes a capability");
    assert_eq!(capability.row_id(), "macos26-m1pro-16gb");
    assert_eq!(
        capability.memory_profile(),
        PlatformMemoryProfileName::UnifiedMemory
    );
    assert_eq!(capability.detected_memory_bytes(), Some(17_179_869_184));
    assert_eq!(capability.row_memory_budget_bytes(), 17_179_869_184);
    assert_eq!(capability.max_resident_model_memory(), 17_179_869_184);
}

#[test]
fn m4_pro_resolves_to_the_family_preview_with_a_conservative_memory_ceiling() {
    let report =
        identify_platform(&sources("macos26-m4pro-24gb")).expect("M4 Pro detection succeeds");
    let registry = registry();
    let detected = report.detected_platform();
    let RowMatch::Supported(row) = registry.resolve(&detected) else {
        panic!("M4 Pro must resolve to the M-series Preview row");
    };
    assert_eq!(row.row_id(), "macos26-apple-m-series-preview");

    let capability = registry
        .resolved_capability(&report)
        .expect("the family Preview row publishes a bounded capability");
    assert_eq!(
        capability.detected_memory_bytes(),
        Some(24_u64 * 1024 * 1024 * 1024)
    );
    assert_eq!(
        capability.row_memory_budget_bytes(),
        16_u64 * 1024 * 1024 * 1024
    );
    assert_eq!(
        capability.max_resident_model_memory(),
        16_u64 * 1024 * 1024 * 1024,
        "a larger M-series pool cannot exceed the compatibility envelope"
    );
}

#[test]
fn other_recognized_m_series_chips_use_the_family_preview() {
    let registry = registry();
    for name in ["macos26-m2pro-16gb", "macos26-m3max-36gb"] {
        let report =
            identify_platform(&sources(name)).unwrap_or_else(|err| panic!("{name}: {err}"));
        let RowMatch::Supported(row) = registry.resolve(&report.detected_platform()) else {
            panic!("{name} must resolve to the M-series Preview row");
        };
        assert_eq!(row.row_id(), "macos26-apple-m-series-preview");
        assert_eq!(
            registry
                .resolved_capability(&report)
                .expect("family row publishes capacity")
                .max_resident_model_memory(),
            16_u64 * 1024 * 1024 * 1024,
            "{name} stays inside the conservative family budget"
        );
    }
}

#[test]
fn apple_chips_outside_the_m_series_family_fail_closed() {
    let report = identify_platform(&sources("macos26-apple-a17pro-unsupported"))
        .expect("the non-M-series Apple identity is detectable");
    let registry = registry();
    assert_eq!(
        registry.resolve(&report.detected_platform()),
        RowMatch::Unsupported(PlatformReason::UnsupportedAcceleratorSku)
    );
    assert!(registry.resolved_capability(&report).is_none());
}

#[test]
fn macos_below_the_row_floor_is_rejected_before_accelerator_admission() {
    let mut old = sources("macos26-m1pro-16gb");
    old.sw_vers_product_version = Some("25.6.0".to_string());
    let report = identify_platform(&old).expect("older macOS is still detectable");
    assert_eq!(
        registry().resolve(&report.detected_platform()),
        RowMatch::Unsupported(PlatformReason::UnsupportedOsVersion)
    );
}

#[test]
fn resolved_memory_never_exceeds_the_row_budget_or_detected_capacity() {
    let registry = registry();

    let mut larger = sources("macos26-m1pro-16gb");
    larger.hw_memsize = Some((32_u64 * 1024 * 1024 * 1024).to_string());
    let larger_report = identify_platform(&larger).expect("larger pool detects");
    let larger_capability = registry
        .resolved_capability(&larger_report)
        .expect("M1 Pro row resolves");
    assert_eq!(
        larger_capability.max_resident_model_memory(),
        16_u64 * 1024 * 1024 * 1024,
        "more physical memory cannot broaden the validated row"
    );

    let mut smaller = sources("macos26-m1pro-16gb");
    smaller.hw_memsize = Some((8_u64 * 1024 * 1024 * 1024).to_string());
    let smaller_report = identify_platform(&smaller).expect("smaller pool detects");
    let smaller_capability = registry
        .resolved_capability(&smaller_report)
        .expect("M1 Pro row resolves");
    assert_eq!(
        smaller_capability.max_resident_model_memory(),
        8_u64 * 1024 * 1024 * 1024,
        "admission cannot exceed memory actually detected"
    );
}

#[test]
fn missing_malformed_or_zero_memory_is_a_typed_probe_failure() {
    let base = sources("macos26-m1pro-16gb");
    for raw in [None, Some("sixteen gigabytes"), Some("0")] {
        let mut broken = base.clone();
        broken.hw_memsize = raw.map(str::to_string);
        let err = identify_platform(&broken).expect_err("broken memory source must fail closed");
        assert!(
            matches!(err, PlatformProbeError::Unrecognized { ref source_name, .. } if source_name == "hw.memsize"),
            "unexpected error: {err:?}"
        );
    }
}
