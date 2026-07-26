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
    AcceleratorIdentity, CpuArchitecture, CpuVendor, DetectedPlatform, HostIdentity,
    PlatformReason, PlatformRegistry, RowMatch, SupportLevel,
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
        architecture,
        vendor,
        os_name: os_name.to_string(),
        os_version: os_version.to_string(),
        image_identity: image_identity.map(str::to_string),
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
        architecture: row.cpu().architecture,
        vendor: row.cpu().vendors[0],
        os_name: row.os().name.clone(),
        os_version: row.os().version.clone(),
        image_identity: row.os().image_identity.clone(),
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

    // The same row twice: one row id, declared twice.
    let duplicate_id = PlatformRegistry::from_documents(
        [
            (Path::new("a.json"), row.as_str()),
            (Path::new("b.json"), row.as_str()),
        ],
        std::iter::empty(),
    )
    .expect_err("a duplicated row id is ambiguous");
    assert!(duplicate_id.to_string().contains("ambiguous"));

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
        ambiguous.to_string().contains("same platform identity"),
        "reason should name the collision: {ambiguous}"
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
            PlatformReason::UnsupportedCpuVendor,
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
    let candidates = registry.candidates(&host(
        CpuArchitecture::X86_64,
        CpuVendor::Intel,
        "Ubuntu",
        "24.04",
        None,
    ));
    let ids: Vec<&str> = candidates.iter().map(|row| row.row_id()).collect();
    assert_eq!(
        ids,
        [
            "ubuntu2404-x86-a100-40g-a2hg1",
            "ubuntu2404-x86-cpu",
            "ubuntu2404-x86-l4-g2s8"
        ],
        "host identity narrows but cannot decide"
    );

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
