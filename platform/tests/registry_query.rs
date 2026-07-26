// SPDX-License-Identifier: Apache-2.0
//
// Registry loading and query resolution against the committed registry.
//
// The properties under test are the ones a wrong answer would be
// expensive for: the registry fails closed rather than half-loading,
// roadmap targets are unreachable from matching, every Production row
// resolves from its own identity, and a machine that matches nothing is
// told the most specific true reason.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use tensorplate_platform::{
    AcceleratorIdentity, CpuArchitecture, CpuVendor, DetectedArchitecture, DetectedPlatform,
    DetectedVendor, HostIdentity, PlatformReason, PlatformRegistry, PlatformSupportRow, RowMatch,
    SupportLevel,
};

fn registry_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("config/platform")
}

fn registry() -> PlatformRegistry {
    PlatformRegistry::load(&registry_dir()).expect("the committed registry loads")
}

fn host(
    architecture: CpuArchitecture,
    vendor: CpuVendor,
    os_name: &str,
    os_version: &str,
    image_identity: Option<&str>,
) -> HostIdentity {
    HostIdentity {
        architecture: DetectedArchitecture::Known(architecture),
        vendor: DetectedVendor::Known(vendor),
        os_name: os_name.to_string(),
        os_version: os_version.to_string(),
        image_identity: image_identity.map(str::to_string),
        machine_type: None,
    }
}

fn accelerator(sku: &str) -> AcceleratorIdentity {
    AcceleratorIdentity {
        sku: sku.to_string(),
        partitioned: false,
    }
}

/// The detected identity of a committed row, derived from the row itself
/// so the test cannot drift from the registry.
fn identity_of(registry: &PlatformRegistry, row_id: &str) -> DetectedPlatform {
    let row = registry.row(row_id).expect("row is committed");
    let host = HostIdentity {
        architecture: DetectedArchitecture::Known(row.cpu().architecture),
        vendor: DetectedVendor::Known(row.cpu().vendors[0]),
        os_name: row.os().name.clone(),
        os_version: row.os().version.clone(),
        image_identity: row.os().image_identity.clone(),
        machine_type: row.validation_environment().machine_type.clone(),
    };
    match row.accelerator() {
        Some(a) => DetectedPlatform::with_accelerator(host, accelerator(&a.sku)),
        None => DetectedPlatform::host_only(host),
    }
}

#[test]
fn the_committed_registry_loads_completely() {
    let registry = registry();
    assert_eq!(registry.rows().count(), 12, "twelve rows load");
    assert_eq!(
        registry.roadmap_targets().count(),
        4,
        "four roadmap targets load"
    );
    assert_eq!(
        registry.supported_rows().count(),
        7,
        "five Production plus two Preview rows are supported combinations"
    );
}

#[test]
fn loading_fails_closed_on_a_single_invalid_row() {
    // A half-loaded registry would answer "unsupported" for rows that
    // were merely unreadable, so one bad document means no registry.
    let good = std::fs::read_to_string(registry_dir().join("rows/macos26-m1pro-16gb.json"))
        .expect("read a committed row");
    let invalid = good.replace(
        "\"support_level\": \"Production\"",
        "\"support_level\": \"Rumoured\"",
    );
    assert_ne!(invalid, good, "the mutation applied");

    let error = PlatformRegistry::from_documents(
        [
            (Path::new("rows/good.json"), good.as_str()),
            (Path::new("rows/broken.json"), invalid.as_str()),
        ],
        std::iter::empty(),
    )
    .expect_err("one invalid row fails the whole load");
    assert!(
        error.to_string().contains("broken.json"),
        "the error names the offending document: {error}"
    );
}

#[test]
fn colliding_registry_entries_are_rejected_at_load() {
    let row = std::fs::read_to_string(registry_dir().join("rows/ubuntu2404-x86-l4-g2s8.json"))
        .expect("read a committed row");

    // One row id declared twice, with identities distinct enough that the
    // overlap check does not fire first.
    let same_id_other_identity = row.replace("\"version\": \"24.04\"", "\"version\": \"24.10\"");
    let duplicate_id = PlatformRegistry::from_documents(
        [
            (Path::new("a.json"), row.as_str()),
            (Path::new("b.json"), same_id_other_identity.as_str()),
        ],
        std::iter::empty(),
    )
    .expect_err("a duplicated row id is ambiguous");
    assert!(
        duplicate_id.to_string().contains("declared twice"),
        "the duplicate-id branch should fire: {duplicate_id}"
    );

    // Distinct ids, same matchable identity: resolution could not pick
    // one, so the registry refuses to load rather than guessing.
    let twin = row.replace(
        "\"row_id\": \"ubuntu2404-x86-l4-g2s8\"",
        "\"row_id\": \"ubuntu2404-x86-l4-twin\"",
    );
    let ambiguous = PlatformRegistry::from_documents(
        [
            (Path::new("a.json"), row.as_str()),
            (Path::new("b.json"), twin.as_str()),
        ],
        std::iter::empty(),
    )
    .expect_err("two rows matching one identity are ambiguous");
    assert!(
        ambiguous.to_string().contains("can both match"),
        "reason should name the collision: {ambiguous}"
    );
}

#[test]
fn a_wildcard_image_identity_cannot_shadow_a_specific_row() {
    // A row with no image identity matches a host that reports one, so it
    // overlaps a row that pins that identity. Distinct keys would have
    // hidden this and left `.find()` picking by row-id order.
    let pinned =
        std::fs::read_to_string(registry_dir().join("rows/jetson-orin-nano-8gb-jp62.json"))
            .expect("read the Jetson row");
    let mut document: serde_json::Value = serde_json::from_str(&pinned).expect("row parses");
    document["row_id"] = serde_json::json!("jetpack62-generic");
    document["os"]
        .as_object_mut()
        .expect("os object")
        .remove("image_identity");
    let wildcard = serde_json::to_string(&document).expect("serialize");
    assert!(
        !wildcard.contains("image_identity"),
        "the wildcard row drops the image identity"
    );

    let error = PlatformRegistry::from_documents(
        [
            (Path::new("pinned.json"), pinned.as_str()),
            (Path::new("wildcard.json"), wildcard.as_str()),
        ],
        std::iter::empty(),
    )
    .expect_err("a wildcard row overlaps the row it would shadow");
    assert!(error.to_string().contains("can both match"));
}

#[test]
fn an_empty_registry_is_not_a_registry() {
    // Zero rows answers "unsupported" for every machine on earth, which is
    // indistinguishable from a registry that failed to load.
    let error = PlatformRegistry::from_documents(std::iter::empty(), std::iter::empty())
        .expect_err("an empty registry must not load");
    assert!(error.to_string().contains("no platform support rows"));
}

#[test]
fn a_partial_registry_directory_fails_to_load() {
    // An operator renaming a row to `.json.bak`, or a stray subdirectory,
    // must not silently produce a smaller registry.
    let staging = std::env::temp_dir().join(format!(
        "tensorplate-registry-partial-{}",
        std::process::id()
    ));
    let rows = staging.join("rows");
    std::fs::create_dir_all(&rows).expect("create staging rows");
    std::fs::create_dir_all(staging.join("roadmap_targets")).expect("create staging targets");
    let body = std::fs::read_to_string(registry_dir().join("rows/ubuntu2404-x86-cpu.json"))
        .expect("read a committed row");
    std::fs::write(rows.join("ubuntu2404-x86-cpu.json"), &body).expect("write row");
    std::fs::write(rows.join("disabled.json.bak"), &body).expect("write disabled row");

    let error = PlatformRegistry::load(&staging).expect_err("a non-JSON entry must fail the load");
    assert!(
        error.to_string().contains("only JSON documents"),
        "the error should name the offending entry: {error}"
    );
    std::fs::remove_dir_all(&staging).ok();
}

#[test]
fn an_experimental_row_is_not_deployable() {
    // The crate owns the support-level vocabulary, so the registry must
    // agree with `is_supported_combination` about what Experimental means.
    let body = std::fs::read_to_string(registry_dir().join("rows/ubuntu2404-x86-l4-g2s8.json"))
        .expect("read a committed row")
        .replace(
            "\"support_level\": \"Production\"",
            "\"support_level\": \"Experimental\"",
        );
    let registry = PlatformRegistry::from_documents(
        [(Path::new("row.json"), body.as_str())],
        std::iter::empty(),
    )
    .expect("an Experimental row is a valid row");
    let row = registry.row("ubuntu2404-x86-l4-g2s8").expect("row loaded");
    assert!(!row.is_supported_combination());

    let detected = identity_of(&registry, "ubuntu2404-x86-l4-g2s8");
    let matched = registry.resolve(&detected);
    assert!(
        matches!(matched, RowMatch::Experimental(_)),
        "an Experimental row has its own state ({matched:?})"
    );
    assert!(
        !matched.is_supported(),
        "deployment must not proceed on an Experimental row"
    );
    assert_eq!(
        matched.reason(),
        None,
        "Experimental is not Planned; the frozen vocabulary has no value for it"
    );
    assert_eq!(
        matched.row().map(PlatformSupportRow::row_id),
        Some("ubuntu2404-x86-l4-g2s8"),
        "the exact match is preserved"
    );
}

#[test]
fn the_reason_names_the_dimension_that_actually_differs() {
    // A machine one CPU vendor away from a row must be told about the
    // vendor, not about its accelerator: the L4 row names an Intel host.
    let registry = registry();
    let mut detected = identity_of(&registry, "ubuntu2404-x86-l4-g2s8");
    detected.host.vendor = DetectedVendor::Known(CpuVendor::Amd);
    assert_eq!(
        registry.resolve(&detected).reason(),
        Some(PlatformReason::UnsupportedCpuVendor),
        "the SKU is named by a row; the vendor is what differs"
    );

    // Same for an OS version: the SKU is right, the version is not.
    let mut detected = identity_of(&registry, "ubuntu2404-x86-l4-g2s8");
    detected.host.os_version = "22.04".to_string();
    assert_eq!(
        registry.resolve(&detected).reason(),
        Some(PlatformReason::UnsupportedOsVersion)
    );

    // And a genuinely unknown SKU still reports the accelerator.
    let mut detected = identity_of(&registry, "ubuntu2404-x86-l4-g2s8");
    detected.accelerator.as_mut().expect("accelerator").sku = "NVIDIA L40S".to_string();
    assert_eq!(
        registry.resolve(&detected).reason(),
        Some(PlatformReason::UnsupportedAcceleratorSku)
    );
}

#[test]
fn a_roadmap_target_cannot_shadow_a_row_id() {
    let row = std::fs::read_to_string(registry_dir().join("rows/macos26-m1pro-16gb.json"))
        .expect("read a committed row");
    let target = std::fs::read_to_string(registry_dir().join("roadmap_targets/rocm-mi300x.json"))
        .expect("read a committed target")
        .replace("\"rocm-mi300x\"", "\"macos26-m1pro-16gb\"");

    let error = PlatformRegistry::from_documents(
        [(Path::new("row.json"), row.as_str())],
        [(Path::new("target.json"), target.as_str())],
    )
    .expect_err("a target must not collide with a row id");
    assert!(error.to_string().contains("collides"));
}

#[test]
fn every_committed_row_resolves_from_its_own_identity() {
    let registry = registry();
    for row in registry.rows() {
        let detected = identity_of(&registry, row.row_id());
        let matched = registry.resolve(&detected);
        let resolved = matched
            .row()
            .unwrap_or_else(|| panic!("{} must resolve to a row", row.row_id()));
        assert_eq!(
            resolved.row_id(),
            row.row_id(),
            "{} resolved to the wrong row",
            row.row_id()
        );
        if row.support_level() == SupportLevel::Planned {
            assert!(
                matches!(matched, RowMatch::PlannedNotValidated(_)),
                "{}: a Planned row is defined but not validated",
                row.row_id()
            );
            assert_eq!(
                matched.reason(),
                Some(PlatformReason::RowPlannedNotValidated)
            );
            assert!(!matched.is_supported());
        } else {
            assert!(
                matched.is_supported(),
                "{}: a row carrying a claim resolves as supported",
                row.row_id()
            );
            assert_eq!(matched.reason(), None);
        }
    }
}

#[test]
fn roadmap_targets_are_never_matchable() {
    let registry = registry();
    // The MI300X target names a real accelerator, but no row does: a
    // machine reporting it must be unsupported, never "roadmapped".
    let detected = DetectedPlatform::with_accelerator(
        host(
            CpuArchitecture::X86_64,
            CpuVendor::Amd,
            "Ubuntu",
            "24.04",
            None,
        ),
        accelerator("AMD Instinct MI300X"),
    );
    let matched = registry.resolve(&detected);
    assert_eq!(
        matched.reason(),
        Some(PlatformReason::UnsupportedAcceleratorSku)
    );
    assert!(matched.row().is_none());
    // The target is still catalogued — just not reachable from matching.
    assert!(registry.roadmap_target("rocm-mi300x").is_some());
}

#[test]
fn a_partitioned_accelerator_is_rejected_before_its_sku_is_considered() {
    let registry = registry();
    let mut detected = identity_of(&registry, "ubuntu2404-x86-a100-40g-a2hg1");
    detected
        .accelerator
        .as_mut()
        .expect("the A100 row has an accelerator")
        .partitioned = true;

    let matched = registry.resolve(&detected);
    assert_eq!(matched.reason(), Some(PlatformReason::MigModeEnabled));
    assert!(
        !matched.is_supported(),
        "a partitioned instance of a supported SKU is still unsupported"
    );
}

#[test]
fn unmatched_identities_get_the_most_specific_reason() {
    let registry = registry();
    let cases = [
        (
            "arm64 host reporting an x86-only vendor",
            DetectedPlatform::host_only(host(
                CpuArchitecture::Arm64,
                CpuVendor::Amd,
                "Ubuntu",
                "24.04",
                None,
            )),
            PlatformReason::UnsupportedCpuArch,
        ),
        (
            "unsupported OS version",
            DetectedPlatform::host_only(host(
                CpuArchitecture::X86_64,
                CpuVendor::Intel,
                "Ubuntu",
                "20.04",
                None,
            )),
            PlatformReason::UnsupportedOsVersion,
        ),
        (
            "unknown OS",
            DetectedPlatform::host_only(host(
                CpuArchitecture::X86_64,
                CpuVendor::Intel,
                "Windows",
                "11",
                None,
            )),
            PlatformReason::UnsupportedOsVersion,
        ),
        (
            "unknown accelerator SKU on a known host",
            DetectedPlatform::with_accelerator(
                host(
                    CpuArchitecture::X86_64,
                    CpuVendor::Intel,
                    "Ubuntu",
                    "24.04",
                    None,
                ),
                accelerator("NVIDIA A100-SXM4-80GB"),
            ),
            PlatformReason::UnsupportedAcceleratorSku,
        ),
        (
            "accelerator-less host where every row declares one",
            DetectedPlatform::host_only(host(
                CpuArchitecture::Arm64,
                CpuVendor::Apple,
                "macOS",
                "26",
                None,
            )),
            PlatformReason::UnsupportedAcceleratorSku,
        ),
    ];

    for (label, detected, expected) in cases {
        let matched = registry.resolve(&detected);
        assert_eq!(
            matched.reason(),
            Some(expected),
            "{label}: wrong reason ({matched:?})"
        );
        assert!(matched.row().is_none(), "{label}: nothing should match");
    }
}

#[test]
fn a_wrong_image_identity_does_not_match_a_row_that_requires_one() {
    let registry = registry();
    // The Jetson row pins an L4T image identity; the same OS version on a
    // different image is a different platform.
    let mut detected = identity_of(&registry, "jetson-orin-nano-8gb-jp62");
    detected.host.image_identity = Some("L4T r35.0.0 (Ubuntu 20.04 base)".to_string());
    assert_eq!(
        registry.resolve(&detected).reason(),
        Some(PlatformReason::UnsupportedOsVersion)
    );

    // Absent where the row requires one is equally a mismatch.
    detected.host.image_identity = None;
    assert_eq!(
        registry.resolve(&detected).reason(),
        Some(PlatformReason::UnsupportedOsVersion)
    );
}

#[test]
fn host_identity_alone_yields_a_candidate_set_not_a_single_row() {
    let registry = registry();
    // Ubuntu 24.04 on an Intel host is consistent with the L4 row, the
    // A100 row, and the CPU-only row: exactly the ambiguity that makes
    // accelerator identity necessary before a single row can be named.
    let mut on_g2 = host(
        CpuArchitecture::X86_64,
        CpuVendor::Intel,
        "Ubuntu",
        "24.04",
        None,
    );
    on_g2.machine_type = Some("g2-standard-8".to_string());
    let ids: Vec<&str> = registry
        .candidates(&on_g2)
        .iter()
        .map(|row| row.row_id())
        .collect();
    assert_eq!(
        ids,
        ["ubuntu2404-x86-cpu", "ubuntu2404-x86-l4-g2s8"],
        "host identity narrows to the shapes it could be, but cannot decide"
    );

    // A host reporting no machine shape cannot be a row scoped to one, so
    // only the shape-agnostic utility row remains.
    let ids: Vec<&str> = registry
        .candidates(&host(
            CpuArchitecture::X86_64,
            CpuVendor::Intel,
            "Ubuntu",
            "24.04",
            None,
        ))
        .iter()
        .map(|row| row.row_id())
        .collect();
    assert_eq!(ids, ["ubuntu2404-x86-cpu"]);

    // A vendor no row covers narrows to nothing rather than guessing.
    assert!(registry
        .candidates(&host(
            CpuArchitecture::X86_64,
            CpuVendor::Apple,
            "Ubuntu",
            "24.04",
            None
        ))
        .is_empty());
}

#[test]
fn lookup_by_row_id_is_exact() {
    let registry = registry();
    assert!(registry.row("macos26-m1pro-16gb").is_some());
    assert!(
        registry.row("macos26-m1pro").is_none(),
        "a prefix is not a row id"
    );
    assert!(
        registry.row("MACOS26-M1PRO-16GB").is_none(),
        "row ids are case-sensitive"
    );
    assert!(
        registry.row("rocm-mi300x").is_none(),
        "a roadmap target is not reachable through row lookup"
    );
}

#[test]
fn a_row_scoped_to_a_machine_shape_does_not_cover_other_shapes() {
    // The L4 row's evidence was recorded on one GCP machine type. The same
    // GPU in a different chassis is not that row: evidence does not
    // transfer across machine shapes.
    let registry = registry();
    let mut detected = identity_of(&registry, "ubuntu2404-x86-l4-g2s8");
    assert!(
        matches!(registry.resolve(&detected), RowMatch::Supported(_)),
        "the row resolves on its own machine shape"
    );

    detected.host.machine_type = Some("g2-standard-16".to_string());
    let matched = registry.resolve(&detected);
    assert!(
        matches!(matched, RowMatch::OutsideValidatedEnvironment(_)),
        "a different machine shape is outside the row's evidence ({matched:?})"
    );
    assert!(!matched.is_supported());
    assert_eq!(
        matched.row().map(PlatformSupportRow::row_id),
        Some("ubuntu2404-x86-l4-g2s8"),
        "the row is still named, so a caller can say which claim does not transfer"
    );

    // A bare-metal host reporting no machine type at all is equally not
    // the cloud row.
    detected.host.machine_type = None;
    assert!(!registry.resolve(&detected).is_supported());
}

#[test]
fn an_unvalidated_machine_shape_does_not_reach_the_cpu_only_row() {
    // The CPU-only rows name no machine shape, so they are indifferent to
    // it — that is deliberate, and this pins it.
    let registry = registry();
    let mut detected = identity_of(&registry, "ubuntu2404-x86-cpu");
    detected.host.machine_type = Some("some-laptop".to_string());
    assert!(
        registry.resolve(&detected).is_supported(),
        "a row naming no machine shape makes no machine-shape claim"
    );
}

#[test]
fn genuinely_unknown_cpu_values_are_reportable() {
    // A riscv64 or VIA host must be reported as unsupported, not as
    // undetectable: the probe carries the observation through.
    let registry = registry();
    let unknown_arch = DetectedPlatform::host_only(HostIdentity {
        architecture: DetectedArchitecture::Other("riscv64".to_string()),
        vendor: DetectedVendor::Known(CpuVendor::Intel),
        os_name: "Ubuntu".to_string(),
        os_version: "24.04".to_string(),
        image_identity: None,
        machine_type: None,
    });
    assert_eq!(
        registry.resolve(&unknown_arch).reason(),
        Some(PlatformReason::UnsupportedCpuArch)
    );

    let unknown_vendor = DetectedPlatform::host_only(HostIdentity {
        architecture: DetectedArchitecture::Known(CpuArchitecture::X86_64),
        vendor: DetectedVendor::Other("via".to_string()),
        os_name: "Ubuntu".to_string(),
        os_version: "24.04".to_string(),
        image_identity: None,
        machine_type: None,
    });
    assert_eq!(
        registry.resolve(&unknown_vendor).reason(),
        Some(PlatformReason::UnsupportedCpuVendor)
    );
}

#[test]
fn tied_dimensions_resolve_by_the_documented_priority() {
    // A machine that is one dimension from two different rows — the L4 row
    // (vendor) and the CPU-only row (accelerator) — reports the broader
    // dimension, deterministically.
    let registry = registry();
    let mut detected = identity_of(&registry, "ubuntu2404-x86-l4-g2s8");
    detected.host.vendor = DetectedVendor::Known(CpuVendor::Amd);
    assert_eq!(
        registry.resolve(&detected).reason(),
        Some(PlatformReason::UnsupportedCpuVendor),
        "vendor outranks accelerator in the documented priority"
    );
}
