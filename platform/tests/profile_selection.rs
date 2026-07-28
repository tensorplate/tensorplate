// SPDX-License-Identifier: Apache-2.0
//
// Platform-profile selection: the host-level answer.
//
// The property under test is that this stays a *set*. Host identity cannot
// name one row — the L4, A100, and CPU-only Ubuntu 24.04 rows are identical
// at host level — so narrowing to one here would assert a match nobody has
// established. A host matching nothing gets a typed reason instead of an
// empty vector the caller has to interpret.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use tensorplate_platform::{
    identify, CpuArchitecture, HostSources, PlatformReason, PlatformRegistry, ProfileSelection,
};

fn registry() -> PlatformRegistry {
    PlatformRegistry::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config/platform"),
    )
    .expect("registry loads")
}

fn ubuntu_2404_intel() -> HostSources {
    HostSources {
        uname_machine: Some("x86_64".to_string()),
        os_release: Some("NAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\n".to_string()),
        cpuinfo: Some("processor\t: 0\nvendor_id\t: GenuineIntel\n".to_string()),
        ..HostSources::default()
    }
}

#[test]
fn a_host_shared_by_several_rows_selects_all_of_them() {
    let registry = registry();

    // A host reporting no machine shape must not inherit a shape-scoped
    // row: evidence recorded on one machine shape does not transfer. Only
    // the deliberately chassis-independent CPU row is consistent with it.
    let identity = identify(&ubuntu_2404_intel()).expect("detects").identity;
    let selection = registry.select_profile(&identity);
    let ids: Vec<&str> = selection
        .candidates()
        .iter()
        .map(|row| row.row_id())
        .collect();
    assert_eq!(
        ids,
        vec!["ubuntu2404-x86-cpu"],
        "a shapeless host selects only the chassis-independent row"
    );
    assert_eq!(selection.no_match_reason(), None);

    // On a shape a row is scoped to, both that row and the
    // chassis-independent one are consistent — and host identity cannot
    // choose between them, because they differ only by accelerator.
    let mut on_g2 = ubuntu_2404_intel();
    on_g2.gce_machine_type = Some("projects/1/machineTypes/g2-standard-8".to_string());
    let identity = identify(&on_g2).expect("detects").identity;
    let selection = registry.select_profile(&identity);
    let mut ids: Vec<&str> = selection
        .candidates()
        .iter()
        .map(|row| row.row_id())
        .collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec!["ubuntu2404-x86-cpu", "ubuntu2404-x86-l4-g2s8"],
        "host identity alone cannot say whether an L4 is fitted"
    );
    assert!(
        ids.len() > 1,
        "narrowing to one here would assert a match nobody established"
    );
}

#[test]
fn a_host_matching_nothing_gets_a_typed_reason_not_an_empty_set() {
    let registry = registry();

    // Unknown vendor: everything else about this host matches a row.
    let mut hygon = ubuntu_2404_intel();
    hygon.cpuinfo = Some("processor\t: 0\nvendor_id\t: HygonGenuine\n".to_string());
    let identity = identify(&hygon).expect("detects").identity;
    assert_eq!(
        registry.select_profile(&identity).no_match_reason(),
        Some(PlatformReason::UnsupportedCpuVendor)
    );

    // Unknown OS version.
    let mut ubuntu_2604 = ubuntu_2404_intel();
    ubuntu_2604.os_release = Some("NAME=\"Ubuntu\"\nVERSION_ID=\"26.04\"\n".to_string());
    let identity = identify(&ubuntu_2604).expect("detects").identity;
    assert_eq!(
        registry.select_profile(&identity).no_match_reason(),
        Some(PlatformReason::UnsupportedOsVersion)
    );
}

#[test]
fn a_host_level_answer_never_blames_the_accelerator() {
    // Nothing has looked at an accelerator at this point, so
    // `unsupported_accelerator_sku` would be a claim nothing supports.
    let registry = registry();
    for sources in [
        {
            let mut s = ubuntu_2404_intel();
            s.cpuinfo = Some("processor\t: 0\nvendor_id\t: HygonGenuine\n".to_string());
            s
        },
        {
            let mut s = ubuntu_2404_intel();
            s.os_release = Some("NAME=\"Fedora Linux\"\nVERSION_ID=\"41\"\n".to_string());
            s
        },
        {
            let mut s = ubuntu_2404_intel();
            s.uname_machine = Some("riscv64".to_string());
            s
        },
    ] {
        let identity = identify(&sources).expect("detects").identity;
        let reason = registry.select_profile(&identity).no_match_reason();
        assert!(reason.is_some(), "a non-matching host gets a reason");
        assert_ne!(
            reason,
            Some(PlatformReason::UnsupportedAcceleratorSku),
            "host-level selection must not report an accelerator reason"
        );
    }
}

#[test]
fn every_committed_row_is_selected_by_its_own_host_identity() {
    // The standing rule, at profile level: a row nothing can select is a
    // row that will never be matched on the hardware it describes.
    let registry = registry();
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("test/platform/host_identity");

    for entry in std::fs::read_dir(fixture_dir).expect("read fixtures") {
        let path = entry.expect("entry").path();
        let body = std::fs::read_to_string(&path).expect("read fixture");
        let fixture: serde_json::Value = serde_json::from_str(&body).expect("parses");
        let Some(row_id) = fixture["row_id"].as_str() else {
            continue;
        };
        let s = &fixture["sources"];
        let text = |key: &str| {
            s.get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        let sources = HostSources {
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
            gce_machine_type: text("gce_machine_type"),
        };
        let identity = identify(&sources).expect("detects").identity;
        let selection = registry.select_profile(&identity);
        assert!(
            selection
                .candidates()
                .iter()
                .any(|row| row.row_id() == row_id),
            "`{row_id}` must be selected by its own host identity, got {:?}",
            selection
                .candidates()
                .iter()
                .map(|row| row.row_id())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn selection_never_reaches_a_roadmap_target() {
    let registry = registry();
    let identity = identify(&ubuntu_2404_intel()).expect("detects").identity;
    let selection = registry.select_profile(&identity);
    for target in registry.roadmap_targets() {
        assert!(
            !selection
                .candidates()
                .iter()
                .any(|row| row.row_id() == target.target_id()),
            "roadmap target `{}` must never be a candidate",
            target.target_id()
        );
    }
    assert!(matches!(selection, ProfileSelection::Candidates(_)));
    // And the architecture survived normalization into the selection.
    assert_eq!(identity.architecture.known(), Some(CpuArchitecture::X86_64));
}
