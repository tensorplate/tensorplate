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
    AcceleratorIdentity, AcceleratorMatchPolicy, CpuArchitecture, CpuVendor, DetectedArchitecture,
    DetectedPlatform, DetectedVendor, HostIdentity, PlatformReason, PlatformRegistry,
    PlatformSupportRow, RowMatch, SupportLevel,
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
        Some(a) => {
            let sku = match a.match_policy {
                AcceleratorMatchPolicy::Exact => a.sku.as_str(),
                AcceleratorMatchPolicy::Family => "Apple M2 Pro",
            };
            DetectedPlatform::with_accelerator(host, accelerator(sku))
        }
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
        "four Production plus three Preview rows are supported combinations"
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
fn duplicate_family_rows_are_rejected_at_load() {
    let row =
        std::fs::read_to_string(registry_dir().join("rows/macos26-apple-m-series-preview.json"))
            .expect("read the family row");
    let twin = row.replace(
        "\"row_id\": \"macos26-apple-m-series-preview\"",
        "\"row_id\": \"macos26-apple-m-series-twin\"",
    );

    let error = PlatformRegistry::from_documents(
        [
            (Path::new("family.json"), row.as_str()),
            (Path::new("twin.json"), twin.as_str()),
        ],
        std::iter::empty(),
    )
    .expect_err("two family rows at the same priority are ambiguous");
    assert!(error.to_string().contains("same priority"));
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
fn exact_apple_row_precedes_the_m_series_family_fallback() {
    let registry = registry();

    let exact = identity_of(&registry, "macos26-m1pro-16gb");
    assert!(matches!(
        registry.resolve(&exact),
        RowMatch::Supported(row) if row.row_id() == "macos26-m1pro-16gb"
    ));

    let family = identity_of(&registry, "macos26-apple-m-series-preview");
    assert!(matches!(
        registry.resolve(&family),
        RowMatch::Supported(row) if row.row_id() == "macos26-apple-m-series-preview"
    ));
}

#[test]
fn exact_apple_row_also_precedes_a_family_environment_miss() {
    let registry = registry();
    let mut detected = identity_of(&registry, "macos26-m1pro-16gb");
    detected.host.machine_type = Some("virtualized-mac".to_string());

    assert_eq!(
        registry.resolve(&detected),
        RowMatch::OutsideValidatedEnvironment {
            candidate: Some(registry.row("macos26-m1pro-16gb").expect("committed"))
        },
        "the lower-priority family row must not make the exact environment miss ambiguous"
    );
}

#[test]
fn apple_family_matching_covers_each_documented_variant_form() {
    let registry = registry();
    let host = identity_of(&registry, "macos26-apple-m-series-preview").host;
    for sku in ["Apple M1", "Apple M2 Pro", "Apple M3 Max", "Apple M4 Ultra"] {
        let detected = DetectedPlatform::with_accelerator(host.clone(), accelerator(sku));
        assert!(matches!(
            registry.resolve(&detected),
            RowMatch::Supported(row) if row.row_id() == "macos26-apple-m-series-preview"
        ));
    }
}

#[test]
fn apple_family_matching_rejects_near_miss_brand_strings() {
    let registry = registry();
    let host = identity_of(&registry, "macos26-apple-m-series-preview").host;
    for sku in [
        "Apple M",
        "Apple M01",
        "Apple M2 Extreme",
        "Apple M2 Pro engineering sample",
        "apple M2 Pro",
    ] {
        let detected = DetectedPlatform::with_accelerator(host.clone(), accelerator(sku));
        assert_eq!(
            registry.resolve(&detected),
            RowMatch::Unsupported(PlatformReason::UnsupportedAcceleratorSku),
            "`{sku}` must not be normalized into the family"
        );
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
            "unknown accelerator SKU on a validated machine shape",
            DetectedPlatform::with_accelerator(
                HostIdentity {
                    machine_type: Some("g2-standard-8".to_string()),
                    ..host(
                        CpuArchitecture::X86_64,
                        CpuVendor::Intel,
                        "Ubuntu",
                        "24.04",
                        None,
                    )
                },
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
    // The Planned Jetson rows name no machine shape yet, so a JetPack host
    // is consistent with all of them and differs only by accelerator —
    // exactly the ambiguity that makes accelerator identity necessary
    // before a single row can be named.

    let registry = registry();
    let jetson = host(
        CpuArchitecture::Arm64,
        CpuVendor::NvidiaSoc,
        "JetPack",
        "6.2",
        Some("L4T r36.x (Ubuntu 22.04 base)"),
    );
    let ids: Vec<&str> = registry
        .candidates(&jetson)
        .iter()
        .map(|row| row.row_id())
        .collect();
    assert_eq!(
        ids,
        [
            "jetson-agx-orin-32gb",
            "jetson-agx-orin-64gb",
            "jetson-orin-nano-8gb-jp62",
            "jetson-orin-nx-16gb"
        ],
        "host identity narrows but cannot decide between accelerators"
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
        matches!(matched, RowMatch::OutsideValidatedEnvironment { .. }),
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
fn a_chassis_independent_row_makes_no_machine_shape_claim() {
    // The CPU-only rows are deliberately broad: their claim is install,
    // CLI, packaging, and control-plane smoke on any x86_64 Ubuntu host,
    // so they name no machine shape and any such host matches.
    let registry = registry();
    let row = registry
        .row("ubuntu2404-x86-cpu")
        .expect("row is committed");
    assert!(
        row.validation_environment().machine_type.is_none(),
        "a chassis-independent row declares no shape"
    );

    let mut detected = identity_of(&registry, "ubuntu2404-x86-cpu");
    assert!(registry.resolve(&detected).is_supported());
    detected.host.machine_type = Some("someones-laptop".to_string());
    assert!(
        registry.resolve(&detected).is_supported(),
        "a row naming no shape makes no shape claim"
    );
}

#[test]
fn only_shape_scoped_rows_declare_a_machine_type() {
    // Declaring a machine_type pins a row to one exact shape, so the
    // committed registry declares one exactly where the claim is
    // shape-bound: the cloud rows. A value a probe cannot report would
    // make its row permanently unmatchable.
    //
    // Omitting it is NOT the same as placing no constraint. What an
    // omitted machine_type means is decided by `validation_environment.kind`
    // — a physical row still matches only a host reporting no shape. See
    // `AcceptedShapes` in the registry.
    let registry = registry();
    let scoped: Vec<&str> = registry
        .rows()
        .filter(|row| row.validation_environment().machine_type.is_some())
        .map(PlatformSupportRow::row_id)
        .collect();
    assert_eq!(
        scoped,
        [
            "ubuntu2404-x86-a100-40g-a2hg1",
            "ubuntu2404-x86-l4-g2s8",
            "ubuntu2404-x86-rtxpro6000se-g4s48"
        ],
        "only the cloud rows are shape-scoped"
    );
    for row_id in scoped {
        let machine_type = registry
            .row(row_id)
            .expect("row is committed")
            .validation_environment()
            .machine_type
            .clone()
            .expect("scoped row declares a shape");
        assert!(
            machine_type.starts_with(['a', 'g']),
            "{row_id}: the shape is the GCE machine type a metadata probe reports, got \
             `{machine_type}`"
        );
    }
}

#[test]
fn an_environment_only_miss_outranks_a_nearest_miss_on_another_row() {
    // A machine whose hardware matches a shape-scoped row exactly, on the
    // wrong shape, is told about that row — not about some other row's
    // dimension, even though both are one dimension away.
    let registry = registry();
    let mut detected = identity_of(&registry, "ubuntu2404-x86-l4-g2s8");
    detected.host.machine_type = Some("g2-standard-16".to_string());
    let matched = registry.resolve(&detected);
    assert_eq!(
        matched,
        RowMatch::OutsideValidatedEnvironment {
            candidate: Some(registry.row("ubuntu2404-x86-l4-g2s8").expect("committed"))
        },
        "the environment miss is reported ahead of any nearest-miss reason"
    );
    assert_eq!(matched.reason(), None);
}

#[test]
fn an_ambiguous_environment_miss_names_no_row() {
    // Two rows with identical hardware differing only by machine shape:
    // a third shape is outside both, and naming either would be arbitrary.
    let base = std::fs::read_to_string(registry_dir().join("rows/ubuntu2404-x86-l4-g2s8.json"))
        .expect("read the L4 row");
    let mut other: serde_json::Value = serde_json::from_str(&base).expect("row parses");
    other["row_id"] = serde_json::json!("ubuntu2404-x86-l4-g2s16");
    other["validation_environment"]["machine_type"] = serde_json::json!("g2-standard-16");
    let other = serde_json::to_string(&other).expect("serialize");

    let registry = PlatformRegistry::from_documents(
        [
            (Path::new("a.json"), base.as_str()),
            (Path::new("b.json"), other.as_str()),
        ],
        std::iter::empty(),
    )
    .expect("rows differing only by machine shape are distinguishable");

    let mut detected = identity_of(&registry, "ubuntu2404-x86-l4-g2s8");
    detected.host.machine_type = Some("g2-standard-32".to_string());
    let matched = registry.resolve(&detected);
    assert_eq!(
        matched,
        RowMatch::OutsideValidatedEnvironment { candidate: None },
        "with two equally-close rows, no single row may be named"
    );
    assert!(matched.row().is_none());
}

#[test]
fn a_non_canonical_machine_type_is_rejected_by_both_sides() {
    let validator = jsonschema::JSONSchema::compile(
        &serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../config/schemas/platform_support_row.json"),
            )
            .expect("read the row schema"),
        )
        .expect("schema parses"),
    )
    .expect("schema compiles");

    let base = std::fs::read_to_string(registry_dir().join("rows/ubuntu2404-x86-l4-g2s8.json"))
        .expect("read the L4 row");
    let mut document: serde_json::Value = serde_json::from_str(&base).expect("row parses");
    document["validation_environment"]["machine_type"] = serde_json::json!("G2 Standard 8");
    assert!(
        !validator.is_valid(&document),
        "the schema rejects a non-canonical machine type"
    );
    let raw = serde_json::to_string(&document).expect("serialize");
    assert!(
        PlatformSupportRow::from_json(&raw).is_err(),
        "the decoder must reject it too"
    );

    // Omitting it entirely is legitimate: that is how a row says its claim
    // is not scoped to a machine shape.
    let mut unscoped: serde_json::Value = serde_json::from_str(&base).expect("row parses");
    unscoped["validation_environment"]
        .as_object_mut()
        .expect("environment object")
        .remove("machine_type");
    assert!(validator.is_valid(&unscoped));
    let raw = serde_json::to_string(&unscoped).expect("serialize");
    assert!(PlatformSupportRow::from_json(&raw).is_ok());
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

#[test]
fn a_physical_row_and_a_same_sku_cloud_row_are_not_ambiguous() {
    // The load-time overlap check is documented as the exact negation of
    // "resolution is unambiguous". Since a physical row and a shape-scoped
    // cloud row accept disjoint sets of hosts, rejecting the pair would
    // block a legitimate environment separation — the natural way to
    // record the same accelerator validated both in a chassis and on an
    // instance.
    let physical = std::fs::read_to_string(
        registry_dir().join("rows/ubuntu2404-x86-rtxpro6000we-physical.json"),
    )
    .expect("read the physical row");
    let mut document: serde_json::Value = serde_json::from_str(&physical).expect("row parses");
    document["row_id"] = serde_json::json!("ubuntu2404-x86-rtxpro6000we-cloud");
    document["validation_environment"] = serde_json::json!({
        "kind": "cloud_instance",
        "identity": "GCP g4-standard-48 (disposable)",
        "machine_type": "g4-standard-48",
    });
    let cloud = serde_json::to_string(&document).expect("serialize");

    let registry = PlatformRegistry::from_documents(
        [
            (Path::new("physical.json"), physical.as_str()),
            (Path::new("cloud.json"), cloud.as_str()),
        ],
        std::iter::empty(),
    )
    .expect("same accelerator in two environments is not ambiguous");
    assert_eq!(registry.rows().count(), 2);

    // And resolution really is unambiguous: each host reaches exactly one.
    let shaped = HostIdentity {
        machine_type: Some("g4-standard-48".to_string()),
        ..identity_of(&registry, "ubuntu2404-x86-rtxpro6000we-cloud").host
    };
    let candidates: Vec<&str> = registry
        .candidates(&shaped)
        .into_iter()
        .map(PlatformSupportRow::row_id)
        .collect();
    assert_eq!(candidates, vec!["ubuntu2404-x86-rtxpro6000we-cloud"]);

    let unshaped = HostIdentity {
        machine_type: None,
        ..shaped
    };
    let candidates: Vec<&str> = registry
        .candidates(&unshaped)
        .into_iter()
        .map(PlatformSupportRow::row_id)
        .collect();
    assert_eq!(candidates, vec!["ubuntu2404-x86-rtxpro6000we-physical"]);
}
