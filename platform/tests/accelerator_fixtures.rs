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

/// Rows that name a discrete NVIDIA accelerator, which is what this
/// detection path exists for.
fn gpu_rows(registry: &PlatformRegistry) -> Vec<&PlatformSupportRow> {
    registry
        .rows()
        .filter(|row| {
            row.accelerator()
                .is_some_and(|a| a.sku.starts_with("NVIDIA "))
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
fn an_off_matrix_sku_is_unsupported_and_never_a_nearest_match() {
    // The failure this guards against is a near-miss resolving to its
    // closest row: an A100 80GB is one capacity away from a supported
    // A100, and answering `Supported` there would claim evidence that was
    // collected on different silicon.
    let registry = registry();
    let a100 = registry
        .row("ubuntu2404-x86-a100-40g-a2hg1")
        .expect("the A100 row is committed");

    for (name, text) in fixtures() {
        if !name.starts_with("unsupported-") {
            continue;
        }
        let report = identify_accelerator(&sources(&text))
            .unwrap_or_else(|e| panic!("{name}: detection failed: {e}"))
            .unwrap_or_else(|| panic!("{name}: fixture reported no accelerator"));

        // Pair it with a host whose every other dimension is supported, so
        // the accelerator is the only thing that can put it off matrix.
        let detected = DetectedPlatform::with_accelerator(host_of(a100), report.identity.clone());
        match registry.resolve(&detected) {
            RowMatch::Unsupported(PlatformReason::UnsupportedAcceleratorSku) => {}
            other => panic!(
                "{name}: `{}` must be unsupported_accelerator_sku, got {other:?}",
                report.identity.sku
            ),
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
        assert!(
            report.exact.uuid.is_some(),
            "{name}: no device UUID recorded"
        );
    }
}
