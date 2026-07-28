// SPDX-License-Identifier: Apache-2.0
//
// Host identity detection against a fixture for every committed row.
//
// The property under test is the one that silently breaks everything
// downstream: detection must produce the exact strings a row is written
// in. A row whose identity no probe can produce is unmatchable on the very
// hardware it describes, and no test of the row alone would catch it —
// which is why these fixtures are checked against the registry rather than
// against hand-written expectations only.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use serde_json::Value;
use tensorplate_platform::{
    identify, CpuArchitecture, DetectedPlatform, HostSources, PlatformProbeError, PlatformReason,
    PlatformRegistry, RowMatch,
};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("test/platform/host_identity")
}

fn fixtures() -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = std::fs::read_dir(fixture_dir())
        .expect("read fixture dir")
        .map(|entry| {
            let path = entry.expect("dir entry").path();
            let name = path
                .file_stem()
                .and_then(|n| n.to_str())
                .expect("utf-8 name")
                .to_string();
            let body = std::fs::read_to_string(&path).expect("read fixture");
            (name, serde_json::from_str(&body).expect("fixture parses"))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn sources_of(fixture: &Value) -> HostSources {
    let s = &fixture["sources"];
    let text = |key: &str| s.get(key).and_then(Value::as_str).map(str::to_string);
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
        gce_machine_type: text("gce_machine_type"),
    }
}

#[test]
fn every_fixture_detects_the_identity_it_declares() {
    for (name, fixture) in fixtures() {
        let report = identify(&sources_of(&fixture))
            .unwrap_or_else(|e| panic!("{name}: detection failed: {e}"));
        let want = &fixture["expect"];
        let got = &report.identity;

        assert_eq!(
            got.architecture.as_reported(),
            want["architecture"].as_str().expect("architecture"),
            "{name}: architecture"
        );
        assert_eq!(
            got.vendor.as_reported(),
            want["vendor"].as_str().expect("vendor"),
            "{name}: vendor"
        );
        assert_eq!(
            got.os_name,
            want["os_name"].as_str().expect("os_name"),
            "{name}: os_name"
        );
        assert_eq!(
            got.os_version,
            want["os_version"].as_str().expect("os_version"),
            "{name}: os_version"
        );
        assert_eq!(
            got.image_identity.as_deref(),
            want["image_identity"].as_str(),
            "{name}: image_identity"
        );
        assert_eq!(
            got.machine_type.as_deref(),
            want["machine_type"].as_str(),
            "{name}: machine_type"
        );
    }
}

#[test]
fn every_committed_row_has_a_fixture() {
    // A row with no fixture is a row nobody has checked is detectable.
    let registry = PlatformRegistry::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config/platform"),
    )
    .expect("registry loads");
    let have: Vec<String> = fixtures()
        .into_iter()
        .filter_map(|(_, f)| f["row_id"].as_str().map(str::to_string))
        .collect();
    for row in registry.rows() {
        assert!(
            have.iter().any(|id| id == row.row_id()),
            "row `{}` has no host-identity fixture",
            row.row_id()
        );
    }
}

#[test]
fn a_detected_host_identity_is_consistent_with_its_row() {
    // The standing rule: every value a row matches on must be one a probe
    // can actually produce. Checked against the registry's own comparison,
    // not against a restatement of it here.
    let registry = PlatformRegistry::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config/platform"),
    )
    .expect("registry loads");

    for (name, fixture) in fixtures() {
        let Some(row_id) = fixture["row_id"].as_str() else {
            continue;
        };
        let row = registry
            .row(row_id)
            .unwrap_or_else(|| panic!("{name}: names a row that is not committed"));
        let report = identify(&sources_of(&fixture)).expect("detection succeeds");

        assert_eq!(
            report.identity.os_name,
            row.os().name,
            "{name}: detected os_name must equal the row's"
        );
        assert_eq!(
            report.identity.os_version,
            row.os().version,
            "{name}: detected os_version must equal the row's"
        );
        assert_eq!(
            report.identity.image_identity,
            row.os().image_identity,
            "{name}: detected image_identity must equal the row's"
        );
        assert_eq!(
            report.identity.machine_type,
            row.validation_environment().machine_type,
            "{name}: detected machine_type must equal the row's"
        );
        assert_eq!(
            report.identity.architecture.known(),
            Some(row.cpu().architecture),
            "{name}: detected architecture must be one the row names"
        );
        let vendor = report
            .identity
            .vendor
            .known()
            .unwrap_or_else(|| panic!("{name}: vendor must be a known one"));
        assert!(
            row.cpu().vendors.contains(&vendor),
            "{name}: detected vendor `{}` is outside the row's vendor set",
            vendor.as_str()
        );
    }
}

#[test]
fn a_host_identity_resolves_through_the_registry_to_its_own_row() {
    // Host identity alone cannot pick a single row where several share an
    // OS and CPU profile, so this asserts the row is among the candidates
    // rather than that it is the unique match — single-row resolution needs
    // accelerator identity and is owned elsewhere.
    let registry = PlatformRegistry::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config/platform"),
    )
    .expect("registry loads");

    for (name, fixture) in fixtures() {
        let Some(row_id) = fixture["row_id"].as_str() else {
            continue;
        };
        let report = identify(&sources_of(&fixture)).expect("detection succeeds");
        let candidates: Vec<&str> = registry
            .candidates(&report.identity)
            .into_iter()
            .map(tensorplate_platform::PlatformSupportRow::row_id)
            .collect();
        assert!(
            candidates.contains(&row_id),
            "{name}: `{row_id}` must be among its own host candidates, got {candidates:?}"
        );
    }
}

#[test]
fn the_lab_jetson_does_not_currently_match_the_row_it_should_validate() {
    // Documents a known gap rather than hiding it. The Jetson row is
    // spec_authored at JetPack 6.2 / L4T r36.4.x, while every recorded
    // observation of the in-lab Orin Nano reports L4T R36 REV 5.0. Until
    // the device is reflashed or the row is corrected against a real run,
    // the machine that is supposed to validate that row matches no row at
    // all. If this test starts failing, that gap was closed — update it.
    let registry = PlatformRegistry::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config/platform"),
    )
    .expect("registry loads");

    let fixture = fixtures()
        .into_iter()
        .find(|(name, _)| name == "lab-jetson-orin-nano-l4t-r36.5")
        .map(|(_, f)| f)
        .expect("the recorded lab Jetson fixture exists");
    assert_eq!(fixture["matches_row"], Value::Bool(false));

    let report = identify(&sources_of(&fixture)).expect("detection succeeds");
    assert_eq!(
        report.identity.image_identity.as_deref(),
        Some("L4T r36.5.x (Ubuntu 22.04 base)"),
        "the recorded lab device reports the r36.5 line"
    );
    assert_eq!(
        registry.candidates(&report.identity).len(),
        0,
        "no committed row claims the L4T line the lab Jetson actually runs"
    );

    let detected = DetectedPlatform::host_only(report.identity);
    assert!(
        !matches!(registry.resolve(&detected), RowMatch::Supported(_)),
        "an unvalidated L4T line must never resolve as supported"
    );
}

#[test]
fn a_jetson_with_damaged_sources_fails_rather_than_looking_unsupported() {
    // Every committed Jetson row carries an image identity, so a Jetson
    // that cannot produce one matches nothing. Returning that as an
    // ordinary no-match would tell the operator their Jetson is an
    // unsupported platform, when the truth is that a source on it is
    // broken — a different problem with a different fix.
    let base = fixtures()
        .into_iter()
        .find(|(name, _)| name == "jetson-orin-nano-8gb-jp62")
        .map(|(_, f)| f)
        .expect("the Jetson fixture exists");

    let mut unparsable = sources_of(&base);
    unparsable.nv_tegra_release = Some("# something else entirely\n".to_string());
    let err = identify(&unparsable).expect_err("an unreadable L4T release is not a no-match");
    assert!(
        matches!(err, PlatformProbeError::Unrecognized { .. }),
        "expected a typed probe failure, got {err:?}"
    );

    let mut no_base = sources_of(&base);
    no_base.os_release = Some("NAME=\"Ubuntu\"\n".to_string());
    let err = identify(&no_base).expect_err("a missing Ubuntu base is not a no-match");
    assert!(
        matches!(err, PlatformProbeError::Unrecognized { .. }),
        "expected a typed probe failure, got {err:?}"
    );

    // The intact fixture still detects, so the guard rejects damage rather
    // than everything.
    assert!(identify(&sources_of(&base)).is_ok());
}

#[test]
fn an_off_matrix_machine_is_unsupported_not_undetectable() {
    // The crate's own rule, in the form that keeps failing: a machine that
    // is merely not on the matrix must come back as an identity the
    // registry can reject with a typed reason, never as a detection error.
    // `vendor_id` is x86-only, so an arm64 Linux host has no such line at
    // all — requiring one made every arm64 Linux host undetectable and
    // made UnsupportedCpuVendor unreachable off x86.
    let registry = PlatformRegistry::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config/platform"),
    )
    .expect("registry loads");

    let arm_server = HostSources {
        uname_machine: Some("aarch64".to_string()),
        os_release: Some("NAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\n".to_string()),
        cpuinfo: Some("processor\t: 0\nCPU implementer\t: 0x41\nCPU part\t: 0xd0c\n".to_string()),
        ..HostSources::default()
    };
    let report = identify(&arm_server).expect("an arm64 Linux host is detectable");
    assert_eq!(
        report.identity.architecture.known(),
        Some(CpuArchitecture::Arm64)
    );
    assert_eq!(
        report.identity.vendor.known(),
        None,
        "no row names a bare ARM implementer"
    );
    assert!(
        report.identity.vendor.as_reported().contains("0x41"),
        "the unnamed vendor is carried verbatim: {}",
        report.identity.vendor.as_reported()
    );
    let detected = DetectedPlatform::host_only(report.identity);
    assert!(
        matches!(registry.resolve(&detected), RowMatch::Unsupported(_)),
        "an arm64 Linux host must reach a typed no-match, not an error"
    );

    // And an x86 host whose vendor no row names reaches the vendor reason
    // specifically — the reason this branch previously made unreachable.
    let hygon = HostSources {
        uname_machine: Some("x86_64".to_string()),
        os_release: Some("NAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\n".to_string()),
        cpuinfo: Some("processor\t: 0\nvendor_id\t: HygonGenuine\n".to_string()),
        ..HostSources::default()
    };
    let report = identify(&hygon).expect("an unnamed x86 vendor is detectable");
    assert_eq!(report.identity.vendor.as_reported(), "HygonGenuine");
    assert_eq!(
        registry.resolve(&DetectedPlatform::host_only(report.identity)),
        RowMatch::Unsupported(PlatformReason::UnsupportedCpuVendor),
        "an unnamed vendor must reach the vendor reason"
    );

    // The same for an architecture no row names.
    let riscv = HostSources {
        uname_machine: Some("riscv64".to_string()),
        ..arm_server.clone()
    };
    let report = identify(&riscv).expect("a riscv64 host is detectable");
    assert_eq!(report.identity.architecture.known(), None);
    assert_eq!(report.identity.architecture.as_reported(), "riscv64");
    let detected = DetectedPlatform::host_only(report.identity);
    assert!(
        matches!(registry.resolve(&detected), RowMatch::Unsupported(_)),
        "an unnamed architecture must reach a typed no-match, not an error"
    );
}

#[test]
fn a_jetson_without_the_jetpack_package_still_matches_its_row() {
    // The nvidia-jetpack metapackage is absent on a BSP-flashed rootfs, a
    // Yocto image, and inside l4t containers. Such a device is still the
    // machine its row describes, so the L4T line it does report has to
    // carry it — otherwise a correctly flashed Jetson resolves as an
    // unsupported OS version.
    let registry = PlatformRegistry::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config/platform"),
    )
    .expect("registry loads");

    let fixture = fixtures()
        .into_iter()
        .find(|(name, _)| name == "jetson-orin-nano-8gb-jp62")
        .map(|(_, f)| f)
        .expect("the Jetson fixture exists");

    let mut without_package = sources_of(&fixture);
    without_package.nvidia_jetpack_version = None;

    let report = identify(&without_package).expect("detection succeeds");
    assert_eq!(
        report.identity.os_version, "6.2",
        "the L4T line names its JetPack release"
    );
    assert_eq!(
        report.identity.image_identity.as_deref(),
        Some("L4T r36.4.x (Ubuntu 22.04 base)")
    );
    assert!(
        registry
            .candidates(&report.identity)
            .into_iter()
            .any(|row| row.row_id() == "jetson-orin-nano-8gb-jp62"),
        "the row must still be a candidate without the package"
    );

    // An L4T line this release has not been told about must not be guessed
    // into a JetPack version, or a device would match a row it was never
    // validated against.
    let mut unknown_line = without_package.clone();
    unknown_line.nv_tegra_release = Some("# R38 (release), REVISION: 1.0\n".to_string());
    let report = identify(&unknown_line).expect("detection succeeds");
    assert_ne!(
        report.identity.os_version, "6.2",
        "an unmapped L4T line must not borrow a JetPack version"
    );
    assert_eq!(registry.candidates(&report.identity).len(), 0);
}

#[test]
fn exact_facts_keep_the_precision_matching_discards() {
    // Evidence recording needs the full version and build; matching
    // deliberately does not. Both must come out of one detection pass.
    let fixture = fixtures()
        .into_iter()
        .find(|(name, _)| name == "macos26-m1pro-16gb")
        .map(|(_, f)| f)
        .expect("the M1 Pro fixture exists");
    let report = identify(&sources_of(&fixture)).expect("detection succeeds");

    assert_eq!(report.identity.os_version, "26", "the row-comparable value");
    assert_eq!(
        report.exact.os_version.as_deref(),
        Some("26.5.2"),
        "the exact value, for evidence"
    );
    assert_eq!(report.exact.os_build.as_deref(), Some("25F84"));
    assert_eq!(report.exact.reported_machine.as_deref(), Some("arm64"));

    let jetson = fixtures()
        .into_iter()
        .find(|(name, _)| name == "jetson-orin-nano-8gb-jp62")
        .map(|(_, f)| f)
        .expect("the Jetson fixture exists");
    let report = identify(&sources_of(&jetson)).expect("detection succeeds");
    assert_eq!(
        report.identity.image_identity.as_deref(),
        Some("L4T r36.4.x (Ubuntu 22.04 base)"),
        "matching sees the minor line"
    );
    assert_eq!(
        report.exact.l4t_release.as_deref(),
        Some("r36.4.3"),
        "evidence sees the exact patch"
    );
    assert_eq!(
        report.exact.device_model.as_deref(),
        Some("NVIDIA Jetson Orin Nano Engineering Reference Developer Kit Super"),
        "the device-tree NUL terminator is stripped"
    );
}
