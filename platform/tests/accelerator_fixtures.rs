// SPDX-License-Identifier: Apache-2.0
//
// Accelerator detection against a recorded `nvidia-smi` answer for every
// GPU row, plus the SKUs that must NOT resolve to one.
//
// Same property as the host fixtures, one dimension over: detection has to
// produce the exact string a row is written in. A row whose SKU no probe
// can produce is unmatchable on the very card it describes, and no test of
// the row alone would catch it — so these are checked by resolving them
// against the real registry rather than against hand-written expectations.
//
// The host half of each case is taken from the row itself, so a fixture
// only ever exercises the accelerator dimension.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use tensorplate_protocol::platform_memory_profile::PlatformMemoryProfileName;

use tensorplate_platform::{
    identify_accelerator, AcceleratorSources, DetectedArchitecture, DetectedPlatform,
    DetectedVendor, HostIdentity, PlatformReason, PlatformRegistry, PlatformSupportRow, RowMatch,
};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn fixture_dir() -> PathBuf {
    repo_root().join("test/platform/accelerator")
}

fn registry() -> PlatformRegistry {
    PlatformRegistry::load(&repo_root().join("config/platform")).expect("registry loads")
}

/// Every fixture, by file stem. `PROVENANCE.md` is documentation, not a case.
fn fixtures() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::fs::read_dir(fixture_dir())
        .expect("read fixture dir")
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("txt"))
        .map(|path| {
            let name = path
                .file_stem()
                .and_then(|n| n.to_str())
                .expect("utf-8 name")
                .to_string();
            (name, std::fs::read_to_string(&path).expect("read fixture"))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn sources(text: &str) -> AcceleratorSources {
    AcceleratorSources {
        nvidia_smi_query: Some(text.to_string()),
    }
}

const SYNTHETIC_UUID_PREFIX: &str = "GPU-00000000-0000-0000-0000-";

/// Existing transcribed fixtures predate the reserved all-zero namespace,
/// but their UUIDs were documented as synthetic when introduced. Keeping
/// the exact pairs here makes that legacy set closed: a new fixture or a
/// changed UUID must use the visibly synthetic namespace instead of adding
/// another live-looking identifier.
fn publication_safe_uuid(name: &str, uuid: &str) -> bool {
    if uuid
        .strip_prefix(SYNTHETIC_UUID_PREFIX)
        .is_some_and(|suffix| suffix.len() == 12 && suffix.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return true;
    }

    matches!(
        (name, uuid),
        (
            "mig-enabled-a100-40g" | "ubuntu2404-x86-a100-40g-a2hg1",
            "GPU-6b8e2a41-93cd-4a0f-b2e7-5f1c9d3a7e02"
        ) | (
            "ubuntu2404-x86-l4-g2s8",
            "GPU-1d3f0c1e-6a2b-4f77-9d51-8c0b2a6e4f10"
        ) | (
            "ubuntu2404-x86-rtxpro6000se-g4s48",
            "GPU-9f4a7c25-1e88-46b3-a0d9-2c7b5e8f31a4"
        ) | (
            "ubuntu2404-x86-rtxpro6000we-physical",
            "GPU-4b1d8e70-2f95-4c31-86a7-0d5e9b2c8f14"
        ) | (
            "unsupported-a100-80gb",
            "GPU-2a7c4e19-8b03-4d6f-91ae-7c5d0f2b8e63"
        ) | (
            "unsupported-rtx-6000-ada",
            "GPU-8c2f5a63-4d19-4e7b-b085-9a3c1e6d4f27"
        ) | (
            "unsupported-rtx-a6000",
            "GPU-3e9b1d47-5c62-4a08-8fd3-1b6e9a4c7052"
        )
    )
}

/// The host identity a row describes, so a case exercises only the
/// accelerator dimension. Taking it from the row rather than hand-writing
/// it keeps the two from drifting.
fn host_of(row: &PlatformSupportRow) -> HostIdentity {
    let cpu = row.cpu();
    HostIdentity {
        architecture: DetectedArchitecture::Known(cpu.architecture),
        // Any vendor the row covers will do; the accelerator is what these
        // cases vary.
        vendor: DetectedVendor::Known(*cpu.vendors.first().expect("a row names a vendor")),
        os_name: row.os().name.clone(),
        os_version: row.os().version.clone(),
        image_identity: row.os().image_identity.clone(),
        machine_type: row.validation_environment().machine_type.clone(),
    }
}

/// Rows with a discrete accelerator, which is what this detection path
/// exists for.
///
/// Selected on the row's own `memory_profile`, never on the SKU string.
/// Deriving the coverage set from the very value the fixtures exist to
/// check would let a row opt out of coverage by spelling its SKU
/// differently — and a row nobody has a fixture for is a row nobody has
/// checked is detectable. A future non-NVIDIA discrete row fails here
/// loudly, which is the right outcome: someone must decide which probe
/// covers it.
fn gpu_rows(registry: &PlatformRegistry) -> Vec<&PlatformSupportRow> {
    registry
        .rows()
        .filter(|row| {
            row.accelerator()
                .is_some_and(|a| a.memory_profile == PlatformMemoryProfileName::DiscreteGpu)
        })
        .collect()
}

#[test]
fn every_row_fixture_resolves_to_the_row_it_is_named_for() {
    let registry = registry();
    for (name, text) in fixtures() {
        let Some(row) = registry.row(&name) else {
            continue; // a negative case; covered below
        };
        let report = identify_accelerator(&sources(&text))
            .unwrap_or_else(|e| panic!("{name}: detection failed: {e}"))
            .unwrap_or_else(|| panic!("{name}: fixture reported no accelerator"));

        let expected_sku = row.accelerator().expect("a GPU row").sku.as_str();
        assert_eq!(
            report.identity.sku, expected_sku,
            "{name}: detection must produce the exact string the row is written in"
        );

        // A Planned row is defined and detectable but carries no evidence,
        // so it resolves as PlannedNotValidated. Both are "this fixture
        // reached its own row"; the support claim is the row's business.
        let detected = DetectedPlatform::with_accelerator(host_of(row), report.identity);
        let matched = match registry.resolve(&detected) {
            RowMatch::Supported(matched) | RowMatch::PlannedNotValidated(matched) => matched,
            other => panic!("{name}: expected its own row, got {other:?}"),
        };
        assert_eq!(matched.row_id(), name, "{name}: resolved to the wrong row");
    }
}

#[test]
fn every_gpu_row_has_a_fixture() {
    // A GPU row with no fixture is a row nobody has checked is detectable
    // from what the tool actually prints.
    let registry = registry();
    let have: Vec<String> = fixtures().into_iter().map(|(name, _)| name).collect();
    for row in gpu_rows(&registry) {
        assert!(
            have.iter().any(|name| name == row.row_id()),
            "row `{}` names an NVIDIA accelerator but has no detection fixture",
            row.row_id()
        );
    }
}

#[test]
fn an_off_matrix_sku_is_unsupported_against_every_row_not_just_one() {
    // The failure this guards against is a near-miss resolving to its
    // closest row. Pairing every off-matrix card with a single row's host
    // would only ever exercise the near-miss for that one row: an RTX 6000
    // Ada is a near-miss for the RTX PRO rows, not for the A100. So each
    // one is resolved against the host of every GPU row.
    let registry = registry();
    let gpu_rows = gpu_rows(&registry);
    assert!(gpu_rows.len() > 1, "this case needs more than one GPU row");

    for (name, text) in fixtures() {
        if !name.starts_with("unsupported-") {
            continue;
        }
        let report = identify_accelerator(&sources(&text))
            .unwrap_or_else(|e| panic!("{name}: detection failed: {e}"))
            .unwrap_or_else(|| panic!("{name}: fixture reported no accelerator"));

        for row in &gpu_rows {
            let detected =
                DetectedPlatform::with_accelerator(host_of(row), report.identity.clone());
            match registry.resolve(&detected) {
                RowMatch::Unsupported(PlatformReason::UnsupportedAcceleratorSku) => {}
                other => panic!(
                    "{name}: `{}` against the {} host must be unsupported_accelerator_sku, \
                     got {other:?}",
                    report.identity.sku,
                    row.row_id()
                ),
            }
        }
    }
}

#[test]
fn a_partitioned_device_is_rejected_before_its_sku_is_considered() {
    // The MIG fixture is the supported A100 with partitioning on. It must
    // not resolve to that row, and the reason must name partitioning
    // rather than the SKU — the card is fine, its configuration is not.
    let registry = registry();
    let text = std::fs::read_to_string(fixture_dir().join("mig-enabled-a100-40g.txt"))
        .expect("read MIG fixture");
    let report = identify_accelerator(&sources(&text))
        .expect("detection succeeds")
        .expect("one device");
    assert!(
        report.identity.partitioned,
        "the MIG fixture must detect as partitioned"
    );

    let row = registry
        .row("ubuntu2404-x86-a100-40g-a2hg1")
        .expect("the A100 row is committed");
    assert_eq!(
        report.identity.sku,
        row.accelerator().expect("a GPU row").sku.as_str(),
        "the MIG fixture must carry a SUPPORTED sku, or it proves nothing"
    );

    let detected = DetectedPlatform::with_accelerator(host_of(row), report.identity);
    match registry.resolve(&detected) {
        RowMatch::Unsupported(PlatformReason::MigModeEnabled) => {}
        other => panic!("a partitioned supported card must report mig_mode_enabled, got {other:?}"),
    }

    // The ordering claim needs a card that would fail the SKU comparison
    // too. If partitioning were checked after the SKU, this would come
    // back unsupported_accelerator_sku and the operator would be sent to
    // replace hardware that is fine.
    let off_matrix = std::fs::read_to_string(fixture_dir().join("unsupported-a100-80gb.txt"))
        .expect("read off-matrix fixture")
        .replace("Disabled", "Enabled");
    let report = identify_accelerator(&sources(&off_matrix))
        .expect("detection succeeds")
        .expect("one device");
    assert!(report.identity.partitioned);
    let detected = DetectedPlatform::with_accelerator(host_of(row), report.identity);
    match registry.resolve(&detected) {
        RowMatch::Unsupported(PlatformReason::MigModeEnabled) => {}
        other => {
            panic!("partitioning must be reported before the SKU is considered, got {other:?}")
        }
    }
}

#[test]
fn a_card_whose_framebuffer_differs_from_its_row_still_matches() {
    // Memory is recorded but never matched on, and this is the case that
    // proves why. An L4's row records 24 GiB nominal; the card reports a
    // usable framebuffer well under that. If memory were a match dimension
    // the supported card would miss its own row.
    //
    // Deliberately NOT asserted as "reported is always less than nominal":
    // an A100 40GB reports exactly its nominal 40 GiB, so that would be a
    // generalization from one card rather than a property.
    let registry = registry();
    let row = registry
        .row("ubuntu2404-x86-l4-g2s8")
        .expect("the L4 row is committed");
    let text = std::fs::read_to_string(fixture_dir().join("ubuntu2404-x86-l4-g2s8.txt"))
        .expect("read L4 fixture");
    let report = identify_accelerator(&sources(&text))
        .expect("detection succeeds")
        .expect("one device");

    let observed = report.exact.memory_total_bytes.expect("a framebuffer");
    let nominal = row.accelerator().expect("a GPU row").memory_bytes;
    assert_ne!(
        observed, nominal,
        "this case only means something while the two differ"
    );

    let detected = DetectedPlatform::with_accelerator(host_of(row), report.identity);
    assert!(
        matches!(registry.resolve(&detected), RowMatch::Supported(matched) if matched.row_id() == "ubuntu2404-x86-l4-g2s8"),
        "a card must match its row even though its framebuffer is not the row's nominal capacity"
    );
}

#[test]
fn detection_records_the_facts_an_evidence_run_needs() {
    // PR-14 commits these as row facts, so losing one silently would be
    // discovered on a GCP instance rather than here.
    for (name, text) in fixtures() {
        let report = identify_accelerator(&sources(&text))
            .expect("detection succeeds")
            .expect("one device");
        assert!(
            !report.exact.reported_name.is_empty(),
            "{name}: no product name recorded"
        );
        assert!(
            report.exact.driver_version.is_some(),
            "{name}: no driver version recorded"
        );
        let uuid = report
            .exact
            .uuid
            .as_deref()
            .unwrap_or_else(|| panic!("{name}: no device UUID recorded"));
        assert!(
            publication_safe_uuid(&name, uuid),
            "{name}: public fixture UUID must use the reserved synthetic namespace; \
             retain raw device identifiers only in private evidence"
        );
    }
}
