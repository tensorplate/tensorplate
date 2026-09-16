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
    identify, identify_accelerator, identify_jetson_accelerator, nvidia_pci_functions,
    AcceleratorSources, CpuArchitecture, DetectedPlatform, HostSources, MachineTypeRecord,
    MachineTypeSource, PlatformProbeError, PlatformReason, PlatformRegistry, RecordWrite, RowMatch,
    SystemHostProbe,
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
        hw_memsize: text("hw_memsize"),
        dmi_product_name: text("dmi_product_name"),
        gce_machine_type: text("gce_machine_type"),
        machine_type_record: text("machine_type_record"),
        boot_id: text("boot_id"),
        proc_meminfo: text("proc_meminfo"),
        pci_devices: text("pci_devices"),
    }
}

const REDACTED_GCE_PROJECT: &str = "REDACTED";
const LEGACY_SYNTHETIC_GCE_PROJECT: &str = "928311501586";
const SYNTHETIC_UUID_PREFIX: &str = "GPU-00000000-0000-0000-0000-";

#[test]
fn published_host_fixtures_do_not_carry_live_identifiers() {
    // The parser reads only the terminal machine type, so the project slot
    // is never evidence and every new public GCP fixture must redact it.
    // The canonical L4 fixture keeps its exact pre-rule synthetic value on
    // this branch to avoid colliding with the recording PR that replaces it;
    // no other fixture may use the legacy exception.
    for (name, fixture) in fixtures() {
        if let Some(machine_type) = fixture["sources"]["gce_machine_type"].as_str() {
            let project = machine_type
                .strip_prefix("projects/")
                .and_then(|rest| rest.split_once("/machineTypes/"))
                .map_or_else(
                    || panic!("{name}: malformed GCE machine type"),
                    |(project, _)| project,
                );
            let legacy_l4 =
                name == "ubuntu2404-x86-l4-g2s8" && project == LEGACY_SYNTHETIC_GCE_PROJECT;
            assert!(
                project == REDACTED_GCE_PROJECT || legacy_l4,
                "{name}: public fixture contains an unapproved GCP project identifier; \
                 use projects/REDACTED and retain the raw value privately"
            );
        }

        if let Some(note) = fixture["provenance_note"].as_str() {
            for token in note.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-')) {
                let Some(suffix) = token.strip_prefix(SYNTHETIC_UUID_PREFIX) else {
                    assert!(
                        !token.starts_with("GPU-"),
                        "{name}: provenance note contains a live-looking device UUID; \
                         use the reserved synthetic namespace"
                    );
                    continue;
                };
                assert!(
                    suffix.len() == 12 && suffix.bytes().all(|b| b.is_ascii_hexdigit()),
                    "{name}: malformed synthetic device UUID"
                );
            }
        }
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
fn the_dlvm_recording_resolves_to_the_l4_row_it_covers() {
    // The DLVM recording is the L4 row's second covered boot path, and its
    // accelerator fixture is not named for a row, so the filename-mapped
    // accelerator tests skip it. This is the explicit pairing: the
    // recording's own host sources and raw `nvidia-smi` answer, driven
    // through full detection, must resolve to the row the coverage claim
    // names -- and the recorded SKU must be the row's, byte for byte. A
    // wrong SKU, or a MIG column reading `Enabled`, fails here.
    let registry = PlatformRegistry::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config/platform"),
    )
    .expect("registry loads");

    let fixture = fixtures()
        .into_iter()
        .find(|(name, _)| name == "dlvm-ubuntu2404-l4-g2s8")
        .map(|(_, f)| f)
        .expect("the recorded DLVM fixture exists");
    assert_eq!(fixture["matches_row"], Value::Bool(true));
    let host = identify(&sources_of(&fixture))
        .expect("detection succeeds")
        .identity;

    let recorded_answer = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("test/platform/accelerator/dlvm-ubuntu2404-l4-g2s8.txt"),
    )
    .expect("the recorded DLVM accelerator answer exists");
    let accelerator = identify_accelerator(&AcceleratorSources {
        nvidia_smi_query: Some(recorded_answer),
    })
    .expect("the recorded answer interprets")
    .expect("the recording carries one accelerator");

    let row = registry
        .row("ubuntu2404-x86-l4-g2s8")
        .expect("the L4 row is committed");
    assert_eq!(
        accelerator.identity.sku,
        row.accelerator().expect("a GPU row").sku,
        "the recorded SKU must be the row's, byte for byte"
    );
    match registry.resolve(&DetectedPlatform::with_accelerator(
        host,
        accelerator.identity,
    )) {
        RowMatch::Supported(resolved) => {
            assert_eq!(resolved.row_id(), "ubuntu2404-x86-l4-g2s8");
        }
        other => panic!("the second boot path must resolve to its row, got {other:?}"),
    }
}

#[test]
fn the_lab_jetson_matches_the_row_it_validates() {
    // This replaces a test asserting the opposite. The row was
    // spec_authored at JetPack 6.2 / L4T r36.4.x while the in-lab Orin Nano
    // reported R36 REV 5.0, so the machine meant to validate the row
    // matched no row at all. That gap is closed: the row now describes the
    // device, and `jetpack_for_l4t` answers for r36.5 -- at the 6.2 feature
    // release, which is what a row records and all the L4T line can say.
    //
    // Kept rather than deleted, and inverted rather than weakened -- the
    // relationship between the lab device and its row is the thing worth
    // watching, in whichever direction it happens to point.
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
    assert_eq!(fixture["matches_row"], Value::Bool(true));

    let report = identify(&sources_of(&fixture)).expect("detection succeeds");
    assert_eq!(
        report.identity.image_identity.as_deref(),
        Some("L4T r36.x (Ubuntu 22.04 base)"),
        "a row names the BSP generation, so the lab device's r36.5 revision \
         resolves to the same identity a common r36.4 install does"
    );
    assert_eq!(
        report.identity.os_version, "6.2",
        "the JetPack release comes from the L4T line -- this board carries no \
         nvidia-jetpack package -- and lands at the feature release a row names"
    );
    assert!(
        registry
            .candidates(&report.identity)
            .into_iter()
            .any(|row| row.row_id() == "jetson-orin-nano-8gb-jp62"),
        "the device must select the row it exists to validate"
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
        Some("L4T r36.x (Ubuntu 22.04 base)")
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
        Some("L4T r36.x (Ubuntu 22.04 base)"),
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

/// The registry every case below resolves against.
fn committed_registry() -> PlatformRegistry {
    PlatformRegistry::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config/platform"),
    )
    .expect("registry loads")
}

#[test]
fn a_jetson_reaches_its_row_without_a_vendor_tool() {
    // The defect this guards: nvidia-smi is the only accelerator probe and
    // JetPack does not ship it, so detection reported no accelerator and
    // every row declaring one mismatched. Every Jetson resolved to no row,
    // and deploy admission refused hardware that had been working.
    let registry = committed_registry();
    let mut checked = 0;
    for (name, fixture) in fixtures() {
        let sources = sources_of(&fixture);
        if sources.nv_tegra_release.is_none() {
            continue;
        }
        let Some(row_id) = fixture["row_id"].as_str() else {
            continue; // the lab device, which matches no row by design
        };
        let identity = identify(&sources)
            .unwrap_or_else(|e| panic!("{name}: detection failed: {e}"))
            .identity;
        let accelerator = identify_jetson_accelerator(&sources)
            .unwrap_or_else(|| panic!("{name}: a Jetson must yield an accelerator identity"));

        let expected = registry
            .row(row_id)
            .and_then(tensorplate_platform::PlatformSupportRow::accelerator)
            .expect("a Jetson row declares an accelerator");
        assert_eq!(
            accelerator.sku, expected.sku,
            "{name}: derivation must produce the exact string the row is written in"
        );

        let detected = DetectedPlatform::with_accelerator(identity, accelerator);
        let matched = match registry.resolve(&detected) {
            RowMatch::Supported(row) | RowMatch::PlannedNotValidated(row) => row,
            other => panic!("{name}: expected its own row, got {other:?}"),
        };
        assert_eq!(
            matched.row_id(),
            row_id,
            "{name}: resolved to the wrong row"
        );
        checked += 1;
    }
    assert!(checked >= 4, "expected every Jetson row; checked {checked}");
}

#[test]
fn a_jetson_module_does_not_inherit_a_sibling_module_row() {
    // The control, and the defect the first attempt at this fix shipped:
    // matching an integrated accelerator on capacity alone compared nothing
    // else about the board, so an Orin NX resolved to the Orin Nano's
    // Production row. The module name must be part of the identity.
    let registry = committed_registry();
    let (_, nano) = fixtures()
        .into_iter()
        .find(|(_, f)| f["row_id"].as_str() == Some("jetson-orin-nano-8gb-jp62"))
        .expect("the Orin Nano fixture is committed");
    let mut sources = sources_of(&nano);

    // Same JetPack, same 8GB class, different module. No row names it.
    sources.device_tree_model =
        Some("NVIDIA Jetson Orin NX Engineering Reference Developer Kit\0".to_string());
    let identity = identify(&sources).expect("detects").identity;
    let accelerator = identify_jetson_accelerator(&sources).expect("a Jetson yields an identity");
    assert_eq!(
        accelerator.sku, "Jetson Orin NX 8GB",
        "the module name must come from the board, not from its capacity"
    );
    let detected = DetectedPlatform::with_accelerator(identity, accelerator);
    assert!(
        matches!(
            registry.resolve(&detected),
            RowMatch::Unsupported(PlatformReason::UnsupportedAcceleratorSku)
        ),
        "a module no row names must be unsupported, never its sibling's row"
    );
}

#[test]
fn a_jetson_that_cannot_report_itself_is_refused_not_left_ungated() {
    // This case previously asserted the opposite — that unreadable sources
    // produce an Err — and that assertion pinned a fail-open into the
    // suite. The caller reads any probe error as "admission disabled", so
    // erroring here takes a Jetson from refused to not gated at all.
    //
    // A genuinely unreadable file cannot reach this function: the probe
    // maps one to `PlatformProbeError::Unreadable` and propagates it before
    // these sources are assembled. What arrives as `None` is an ABSENT
    // source, which is a signal, not a failure.
    let registry = committed_registry();
    let (_, nano) = fixtures()
        .into_iter()
        .find(|(_, f)| f["row_id"].as_str() == Some("jetson-orin-nano-8gb-jp62"))
        .expect("the Orin Nano fixture is committed");

    for (label, mutate) in [
        (
            "no board model",
            Box::new(|s: &mut HostSources| s.device_tree_model = None)
                as Box<dyn Fn(&mut HostSources)>,
        ),
        (
            "no memory total",
            Box::new(|s: &mut HostSources| s.proc_meminfo = None),
        ),
        (
            "meminfo with no MemTotal line",
            Box::new(|s: &mut HostSources| {
                s.proc_meminfo = Some("MemFree:         1234567 kB\n".to_string());
            }),
        ),
        (
            "MemTotal in a unit this does not read",
            Box::new(|s: &mut HostSources| {
                s.proc_meminfo = Some("MemTotal:       7689557 KiB\n".to_string());
            }),
        ),
        (
            "a model that is NUL and whitespace only",
            Box::new(|s: &mut HostSources| s.device_tree_model = Some("   \0".to_string())),
        ),
    ] {
        let mut sources = sources_of(&nano);
        mutate(&mut sources);

        let accelerator = identify_jetson_accelerator(&sources)
            .unwrap_or_else(|| panic!("{label}: a Jetson must still report an accelerator"));
        let identity = identify(&sources)
            .expect("host identity still detects")
            .identity;
        let detected = DetectedPlatform::with_accelerator(identity, accelerator);
        assert!(
            matches!(
                registry.resolve(&detected),
                RowMatch::Unsupported(PlatformReason::UnsupportedAcceleratorSku)
            ),
            "{label}: must be refused; anything else lets the agent skip the gate"
        );
    }

    // And a machine that is not a Jetson yields no identity at all.
    let mut not_jetson = sources_of(&nano);
    not_jetson.nv_tegra_release = None;
    assert!(identify_jetson_accelerator(&not_jetson).is_none());
}

#[test]
fn a_jetson_board_with_no_row_is_refused_not_left_ungated() {
    // The regression this guards is a fail-OPEN, and it is the one this
    // change first shipped. `settle_platform_admission` treats a probe
    // error as "hardware unreadable, admission disabled", so returning Err
    // for a board nobody has a row for would take that machine from
    // refused to not gated at all — inverting the gate on exactly the
    // hardware it exists for.
    let registry = committed_registry();
    let (_, nano) = fixtures()
        .into_iter()
        .find(|(_, f)| f["row_id"].as_str() == Some("jetson-orin-nano-8gb-jp62"))
        .expect("the Orin Nano fixture is committed");

    for (label, mutate) in [
        (
            "a module no row names",
            Box::new(|s: &mut HostSources| {
                s.device_tree_model = Some("NVIDIA Jetson Thor Developer Kit\0".to_string());
                s.proc_meminfo = Some("MemTotal:      125829120 kB\n".to_string());
            }) as Box<dyn Fn(&mut HostSources)>,
        ),
        (
            "a capacity no module ships in",
            Box::new(|s: &mut HostSources| {
                s.proc_meminfo = Some("MemTotal:      125829120 kB\n".to_string());
            }),
        ),
        (
            "a board model with no Jetson token",
            Box::new(|s: &mut HostSources| {
                s.device_tree_model = Some("Some Other ARM64 Board\0".to_string());
            }),
        ),
    ] {
        let mut sources = sources_of(&nano);
        mutate(&mut sources);
        let accelerator = identify_jetson_accelerator(&sources)
            .unwrap_or_else(|| panic!("{label}: a Jetson must still report an accelerator"));

        let identity = identify(&sources).expect("detects").identity;
        let detected = DetectedPlatform::with_accelerator(identity, accelerator);
        assert!(
            matches!(
                registry.resolve(&detected),
                RowMatch::Unsupported(PlatformReason::UnsupportedAcceleratorSku)
            ),
            "{label}: must be refused, never admitted and never ungated"
        );
    }
}

#[test]
fn the_super_variant_is_a_trailing_word_not_a_substring() {
    // `Super` names a module variant and the row spells it last. Testing it
    // as a substring anywhere would let an unrelated board name acquire the
    // variant and land on a Production row it is not.
    let (_, nano) = fixtures()
        .into_iter()
        .find(|(_, f)| f["row_id"].as_str() == Some("jetson-orin-nano-8gb-jp62"))
        .expect("the Orin Nano fixture is committed");

    let sku_for = |model: &str| {
        let mut sources = sources_of(&nano);
        sources.device_tree_model = Some(format!("{model}\0"));
        identify_jetson_accelerator(&sources)
            .expect("a Jetson yields an identity")
            .sku
    };

    assert_eq!(
        sku_for("NVIDIA Jetson Orin Nano Engineering Reference Developer Kit Super"),
        "Jetson Orin Nano 8GB Super"
    );
    assert_eq!(
        sku_for("NVIDIA Jetson Orin Nano Developer Kit"),
        "Jetson Orin Nano 8GB",
        "a board that is not the Super variant must not acquire it"
    );
    // The discriminating case: the module name extracts cleanly as `Orin
    // Nano` (the kit description stops it), the capacity is 8GB, and the
    // word `Super` appears AFTER the module rather than as the trailing
    // variant. Under a substring test this composes
    // `Jetson Orin Nano 8GB Super` — the committed Production row — and a
    // board that is not the Super variant inherits its claim. Under the
    // trailing-word test it composes `Jetson Orin Nano 8GB`, which names no
    // row, so the board is refused.
    let trailing_other_word = sku_for("NVIDIA Jetson Orin Nano Developer Kit Super Edition");
    assert_eq!(
        trailing_other_word, "Jetson Orin Nano 8GB",
        "`Super` before another trailing word is not the variant suffix"
    );
    assert!(
        committed_registry().rows().all(|row| row
            .accelerator()
            .map_or(true, |accelerator| accelerator.sku != trailing_other_word)),
        "`{trailing_other_word}` must name no committed row, so the board is refused"
    );
}

#[test]
fn the_capacity_band_rejects_what_is_outside_it() {
    // The band is load bearing: it is what stops a reported total being
    // rounded onto a capacity the module does not have. Asserted at both
    // edges so widening or deleting it fails here.
    let (_, nano) = fixtures()
        .into_iter()
        .find(|(_, f)| f["row_id"].as_str() == Some("jetson-orin-nano-8gb-jp62"))
        .expect("the Orin Nano fixture is committed");
    let sku_for_kb = |kb: u64| {
        let mut sources = sources_of(&nano);
        sources.proc_meminfo = Some(format!("MemTotal:       {kb} kB\n"));
        identify_jetson_accelerator(&sources)
            .expect("a Jetson yields an identity")
            .sku
    };

    // 8 GiB nominal is 8388608 kB; the band admits [80%, 100%].
    assert_eq!(
        sku_for_kb(8_388_608),
        "Jetson Orin Nano 8GB Super",
        "at nominal"
    );
    assert_eq!(
        sku_for_kb(6_710_887),
        "Jetson Orin Nano 8GB Super",
        "at the floor"
    );
    assert!(
        !sku_for_kb(6_710_886).starts_with("Jetson Orin Nano 8GB"),
        "one byte below the floor must not be rounded onto 8GB"
    );
    assert!(
        !sku_for_kb(8_388_609).starts_with("Jetson Orin Nano 8GB"),
        "above nominal is a different module, not this one"
    );
}

#[test]
fn a_card_with_no_working_driver_is_still_visible_on_the_bus() {
    // The whole point of reading PCI. `nvidia-smi` needs a working driver
    // to answer, so a card whose driver is missing or broken looks exactly
    // like no card at all — and such a host currently resolves to the
    // CPU-only row and deploys as though it had no accelerator.
    //
    // The bus does not care about drivers.
    let bus = "\
0000:00:03.0 0x1af4 0x1000 0x020000
0000:00:04.0 0x10de 0x27b8 0x030000
0000:00:04.1 0x10de 0x22bc 0x040300";
    let found = nvidia_pci_functions(bus);
    assert_eq!(
        found,
        vec!["0000:00:04.0"],
        "the display controller is the accelerator; the audio function on \
         the same board is not a second one"
    );
}

#[test]
fn the_bus_reading_never_reaches_matching() {
    // The bus is recorded and never matched on. If a later change wires it
    // into `HostIdentity`, this fails and whoever did it has to say so
    // deliberately.
    //
    // Said deliberately once already: the NVIDIA display device ids are one
    // of the facts a RECORDED GCE machine type is checked against when the
    // metadata service cannot be reached. They validate that record and
    // never derive a machine type -- and this fixture carries a live
    // metadata answer, so the record path is not taken here at all.
    let (_, fixture) = fixtures()
        .into_iter()
        .find(|(_, f)| f["row_id"].as_str() == Some("ubuntu2404-x86-l4-g2s8"))
        .expect("the L4 fixture is committed");

    let mut with_gpu = sources_of(&fixture);
    with_gpu.pci_devices = Some("0000:00:04.0 0x10de 0x27b8 0x030000".to_string());
    let mut without = sources_of(&fixture);
    without.pci_devices = None;

    let seen = identify(&with_gpu).expect("detects");
    let unseen = identify(&without).expect("detects");

    assert_eq!(
        seen.exact.nvidia_pci_functions,
        vec!["0000:00:04.0"],
        "the fact is recorded"
    );
    assert!(unseen.exact.nvidia_pci_functions.is_empty());
    assert_eq!(
        seen.identity, unseen.identity,
        "and it changes no value matching reads"
    );
}

#[test]
fn a_machine_with_no_pci_bus_is_not_a_machine_with_no_devices() {
    // A Mac and a Jetson have no /sys/bus/pci/devices at all. Absence is a
    // signal here exactly as it is for every other source; the distinction
    // that matters is between that and a bus that exists but cannot be
    // read, which the probe raises rather than reporting as empty.
    assert!(nvidia_pci_functions("").is_empty());
    assert!(
        nvidia_pci_functions("0000:00:03.0 0x1af4 0x1000 0x020000").is_empty(),
        "a bus with no NVIDIA display controller yields none"
    );
    // A malformed line is skipped, not fatal: this is evidence, and one bad
    // line must not discard the devices either side of it.
    let ragged = "garbage\n0000:00:04.0 0x10de 0x27b8 0x030000\nalso garbage";
    assert_eq!(nvidia_pci_functions(ragged), vec!["0000:00:04.0"]);

    // Including a class token that is not ASCII. Slicing bytes off this
    // panicked -- `é` is two bytes, so a byte-length check passes and the
    // split lands mid-character -- which is the opposite of "skipped".
    let non_ascii = "0000:00:04.0 0x10de 0x27b8 0xaé\n0000:00:06.0 0x10de 0x27b8 0x030000";
    assert_eq!(
        nvidia_pci_functions(non_ascii),
        vec!["0000:00:06.0"],
        "a class this cannot parse is skipped, and does not take the bus with it"
    );
}

/// Host fixtures that genuinely are recordings: each was captured from a real
/// machine, and each note says which one and when.
///
/// Closed for the same reason the accelerator UUID allowlist is. A fixture is
/// usually made by copying an existing one and changing a field or two, and a
/// copy of a recording inherits `"provenance": "recorded"` along with the
/// note describing a capture it never had. The three multi-GPU L4 fixtures
/// shipped exactly that way: derived from the `g2-standard-8` recording, they
/// claimed to be recordings of shapes nobody had run, and every test passed.
/// A new real recording belongs here -- adding it is a deliberate act, and
/// it should be one.
const RECORDED_HOST_FIXTURES: [&str; 4] = [
    "dlvm-ubuntu2404-l4-g2s8",
    "lab-jetson-orin-nano-l4t-r36.5",
    "macos26-m1pro-16gb",
    "ubuntu2404-x86-l4-g2s8",
];

#[test]
fn only_real_recordings_claim_to_be_recorded() {
    let claiming: Vec<String> = fixtures()
        .into_iter()
        .filter(|(_, fixture)| fixture["provenance"] == "recorded")
        .map(|(name, _)| name)
        .collect();
    let allowed: Vec<String> = RECORDED_HOST_FIXTURES
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    assert_eq!(
        claiming, allowed,
        "a fixture claiming `recorded` must be a real capture listed in \
         RECORDED_HOST_FIXTURES; a derived fixture is `spec_authored` and \
         must say what it was derived from"
    );
}

// ---------------------------------------------------------------------------
// A GCE machine type without the metadata service.
//
// Every case below starts from the recorded g2-standard-8 L4 fixture. No H100
// host fixture has been recorded, so the a3-highgpu-1g row is not exercised
// here with recorded facts.
// ---------------------------------------------------------------------------

const L4_ROW: &str = "ubuntu2404-x86-l4-g2s8";

/// The record `tensorplate-agent` writes for the recorded L4 fixture, spelled
/// out rather than built by the code under test: 8 `processor` entries,
/// `MemTotal: 32860372 kB`, and one L4 display function.
const L4_RECORD: &str = r#"{
  "schema_version": 2,
  "boot_id": "12345678-1234-4234-8234-123456789abc",
  "machine_type": "g2-standard-8",
  "logical_cpus": 8,
  "mem_total_bytes": 33649020928,
  "nvidia_display_devices": [
    "0x10de:0x27b8"
  ]
}
"#;

fn l4_live_sources() -> HostSources {
    let (_, fixture) = fixtures()
        .into_iter()
        .find(|(name, _)| name == L4_ROW)
        .expect("the recorded L4 fixture is committed");
    HostSources {
        // Synthetic boot identity; the hardware facts remain recorded.
        boot_id: Some("12345678-1234-4234-8234-123456789abc\n".to_string()),
        ..sources_of(&fixture)
    }
}

/// The L4 fixture as an offline probe gathers it: the firmware still says
/// Compute Engine, the metadata service gave no answer, and the record is
/// whatever is on disk.
fn l4_offline_sources(record: Option<&str>) -> HostSources {
    HostSources {
        dmi_product_name: Some("Google Compute Engine\n".to_string()),
        gce_machine_type: None,
        machine_type_record: record.map(str::to_string),
        ..l4_live_sources()
    }
}

fn l4_detected(sources: &HostSources) -> DetectedPlatform {
    let answer = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(format!("test/platform/accelerator/{L4_ROW}.txt")),
    )
    .expect("the recorded L4 accelerator answer exists");
    let card = identify_accelerator(&AcceleratorSources {
        nvidia_smi_query: Some(answer),
    })
    .expect("the recorded answer interprets")
    .expect("the recording carries one accelerator");
    DetectedPlatform::with_accelerator(identify(sources).expect("detects").identity, card.identity)
}

fn unestablished_detail(sources: &HostSources) -> String {
    match identify(sources) {
        Err(PlatformProbeError::IdentityUnestablished { detail, .. }) => detail,
        other => panic!("expected IdentityUnestablished, got {other:?}"),
    }
}

#[test]
fn a_live_answer_outranks_any_record() {
    let mut sources = l4_live_sources();
    sources.dmi_product_name = Some("Google Compute Engine\n".to_string());
    sources.machine_type_record = Some(L4_RECORD.replace("g2-standard-8", "g2-standard-96"));

    let report = identify(&sources).expect("detects");
    assert_eq!(
        report.identity.machine_type.as_deref(),
        Some("g2-standard-8")
    );
    assert_eq!(
        report.exact.machine_type_source,
        Some(MachineTypeSource::GceMetadata)
    );
}

#[test]
fn a_live_answer_naming_no_machine_type_is_refused_not_shapeless() {
    // Not a fallback case either: the service answered. A record that would
    // match is on disk, and still neither it nor "no machine type" is used.
    let mut sources = l4_live_sources();
    sources.dmi_product_name = Some("Google Compute Engine\n".to_string());
    sources.gce_machine_type = Some("projects/REDACTED/machineTypes/".to_string());
    sources.machine_type_record = Some(L4_RECORD.to_string());
    match identify(&sources) {
        Err(PlatformProbeError::Unrecognized {
            source_name,
            detail,
        }) => {
            assert_eq!(source_name, "GCE metadata service");
            assert!(!detail.contains("REDACTED"), "no project slot: {detail}");
        }
        other => panic!("expected Unrecognized, got {other:?}"),
    }
}

#[test]
fn an_offline_instance_with_a_matching_record_resolves_its_row() {
    // The control for every refusal below.
    let sources = l4_offline_sources(Some(L4_RECORD));
    let report = identify(&sources).expect("a matching record establishes the machine type");
    assert_eq!(
        report.identity.machine_type.as_deref(),
        Some("g2-standard-8")
    );
    assert_eq!(
        report.exact.machine_type_source,
        Some(MachineTypeSource::RecordedFromMetadata)
    );
    assert_eq!(
        report.identity,
        identify(&l4_live_sources()).expect("detects").identity,
        "the offline identity is the one the live answer gives"
    );
    match committed_registry().resolve(&l4_detected(&sources)) {
        RowMatch::Supported(row) => assert_eq!(row.row_id(), L4_ROW),
        other => panic!("expected the L4 row, got {other:?}"),
    }
}

#[test]
fn a_copied_record_cannot_validate_an_equal_capacity_machine_after_a_new_boot() {
    let recorded_on = l4_live_sources();
    let record = MachineTypeRecord::for_live_sources(&recorded_on)
        .expect("all recording facts are available")
        .expect("the recorded fixture has live metadata")
        .to_json()
        .expect("serializes");
    let mut new_boot = HostSources {
        dmi_product_name: Some("Google Compute Engine\n".to_string()),
        boot_id: Some("87654321-4321-4321-8321-cba987654321\n".to_string()),
        machine_type_record: Some(record),
        ..recorded_on.clone()
    };
    assert_eq!(new_boot.cpuinfo, recorded_on.cpuinfo);
    assert_eq!(new_boot.proc_meminfo, recorded_on.proc_meminfo);
    assert_eq!(new_boot.pci_devices, recorded_on.pci_devices);

    // Reusing a disk can preserve these capacities while the machine type
    // changes. A live custom type must remain outside the validated row;
    // losing metadata must not let the previous boot's record promote it.
    new_boot.gce_machine_type =
        Some("projects/REDACTED/machineTypes/g2-custom-8-32768".to_string());
    match committed_registry().resolve(&l4_detected(&new_boot)) {
        RowMatch::OutsideValidatedEnvironment {
            candidate: Some(row),
        } => assert_eq!(row.row_id(), L4_ROW),
        other => panic!("a live custom type is outside the validated row: {other:?}"),
    }
    new_boot.gce_machine_type = None;
    let detail = unestablished_detail(&new_boot);
    assert!(
        detail.contains("kernel boot ID changed") && detail.contains("start tensorplate-agent"),
        "an untouched record from another boot must fail closed: {detail}"
    );

    // An online start in the new boot restores a usable record for exactly
    // the newly answered identity, including its unvalidated posture.
    new_boot.gce_machine_type =
        Some("projects/REDACTED/machineTypes/g2-custom-8-32768".to_string());
    new_boot.machine_type_record = Some(
        MachineTypeRecord::for_live_sources(&new_boot)
            .expect("new boot facts are readable")
            .expect("a live custom answer records")
            .to_json()
            .expect("serializes"),
    );
    new_boot.gce_machine_type = None;
    let report = identify(&new_boot).expect("a refreshed record works in its boot");
    assert_eq!(
        report.exact.machine_type_source,
        Some(MachineTypeSource::RecordedFromMetadata)
    );
    match committed_registry().resolve(&l4_detected(&new_boot)) {
        RowMatch::OutsideValidatedEnvironment {
            candidate: Some(row),
        } => assert_eq!(row.row_id(), L4_ROW),
        other => panic!("refreshing must preserve the custom type's posture: {other:?}"),
    }
}

#[test]
fn an_unavailable_boot_id_never_blocks_live_identity_or_permits_a_record() {
    let root = tempfile::tempdir().expect("create staged state");
    let record_path = root
        .path()
        .join("var/lib/tensorplate/state/machine-type.json");
    std::fs::create_dir_all(record_path.parent().expect("state directory"))
        .expect("create state directory");
    std::fs::write(&record_path, L4_RECORD).expect("stage a previous record");
    let probe =
        SystemHostProbe::with_root(root.path().canonicalize().expect("canonical staged root"));
    for boot_id in [
        None,
        Some(""),
        Some("not-a-uuid"),
        Some("00000000-0000-0000-0000-000000000000"),
    ] {
        let mut sources = HostSources {
            dmi_product_name: Some("Google Compute Engine\n".to_string()),
            boot_id: boot_id.map(str::to_string),
            machine_type_record: Some(L4_RECORD.to_string()),
            ..l4_live_sources()
        };
        let live =
            identify(&sources).expect("live metadata establishes identity without boot facts");
        assert_eq!(live.identity.machine_type.as_deref(), Some("g2-standard-8"));
        assert_eq!(
            live.exact.machine_type_source,
            Some(MachineTypeSource::GceMetadata)
        );
        match probe
            .write_machine_type_record(&sources)
            .expect("an unavailable fact is an outcome")
        {
            RecordWrite::FactsUnavailable(fact) => assert!(fact.contains("kernel boot ID")),
            other => panic!("{boot_id:?}: must refuse to write: {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&record_path).expect("read previous record"),
            L4_RECORD
        );
        sources.gce_machine_type = None;
        let detail = unestablished_detail(&sources);
        assert!(
            detail.contains("kernel boot ID") && detail.contains("unavailable"),
            "{boot_id:?}: unavailable never counts as an offline match: {detail}"
        );
    }
}

#[test]
fn legacy_or_invalid_boot_identity_records_are_unusable() {
    let without_boot = L4_RECORD.replace(
        "  \"boot_id\": \"12345678-1234-4234-8234-123456789abc\",\n",
        "",
    );
    for record in [
        without_boot.replace("\"schema_version\": 2", "\"schema_version\": 1"),
        without_boot,
    ] {
        assert!(
            MachineTypeRecord::parse(&record).is_err(),
            "a missing boot binding is not migrated"
        );
        assert!(unestablished_detail(&l4_offline_sources(Some(&record)))
            .contains("recorded machine type is unusable"));
    }
    for boot_id in [
        "",
        "00000000-0000-0000-0000-000000000000",
        "12345678-1234-4234-8234-123456789ABC",
        "12345678_1234-4234-8234-123456789abc",
        "12345678-1234-4234-8234-123456789ab",
        "12345678-1234-4234-8234-123456789abc0",
        "12345678-1234-4234-8234-123456789abg",
        "12345678-1234-4234-8234-123456789abc ",
    ] {
        let record = L4_RECORD.replace("12345678-1234-4234-8234-123456789abc", boot_id);
        assert_eq!(
            MachineTypeRecord::parse(&record).expect_err("invalid record boot identity"),
            "the boot ID is not a canonical UUID",
            "{boot_id:?}"
        );
        assert!(
            unestablished_detail(&l4_offline_sources(Some(&record)))
                .contains("recorded machine type is unusable"),
            "{boot_id:?}"
        );
    }
}

#[test]
fn staged_sources_never_borrow_the_running_hosts_boot_id() {
    let root = tempfile::tempdir().expect("create staged host");
    let dmi = root.path().join("sys/class/dmi/id/product_name");
    std::fs::create_dir_all(dmi.parent().expect("DMI directory")).expect("create DMI directory");
    std::fs::write(dmi, "Google Compute Engine\n").expect("stage GCE firmware");
    let probe = SystemHostProbe::with_root(root.path());
    assert_eq!(probe.sources().expect("read staged files").boot_id, None);

    let boot = root.path().join("proc/sys/kernel/random/boot_id");
    std::fs::create_dir_all(boot.parent().expect("boot ID directory"))
        .expect("create proc directory");
    std::fs::write(boot, "87654321-4321-4321-8321-cba987654321\n").expect("stage boot ID");
    assert_eq!(
        probe
            .sources()
            .expect("read staged boot ID")
            .boot_id
            .as_deref(),
        Some("87654321-4321-4321-8321-cba987654321\n")
    );
}

#[test]
fn an_offline_instance_with_no_record_fails_rather_than_losing_its_shape() {
    let detail = unestablished_detail(&l4_offline_sources(None));
    assert!(
        detail.contains("no machine type has been recorded")
            && detail.contains("start tensorplate-agent"),
        "says what is missing and how to fix it: {detail}"
    );

    // The trap this refuses to fall into. The same host reporting no machine
    // type is not refused: the L4 row's signals are context only, so the
    // shape miss lands on the row as a candidate, and admission lets that
    // through as unvalidated.
    let shapeless = HostSources {
        dmi_product_name: None,
        ..l4_offline_sources(None)
    };
    match committed_registry().resolve(&l4_detected(&shapeless)) {
        RowMatch::OutsideValidatedEnvironment {
            candidate: Some(row),
        } => assert_eq!(row.row_id(), L4_ROW),
        other => panic!("expected an environment-only miss on the L4 row, got {other:?}"),
    }
}

#[test]
fn a_record_is_ignored_off_compute_engine() {
    for dmi in [None, Some("Precision 7960 Tower\n")] {
        let sources = HostSources {
            dmi_product_name: dmi.map(str::to_string),
            ..l4_offline_sources(Some(L4_RECORD))
        };
        let report = identify(&sources)
            .unwrap_or_else(|err| panic!("{dmi:?}: not an instance, so no error: {err}"));
        assert_eq!(report.identity.machine_type, None, "{dmi:?}");
        assert_eq!(report.exact.machine_type_source, None, "{dmi:?}");
    }

    // And no committed fixture changes identity for carrying one: Jetson,
    // macOS and the physical rows have no Compute Engine firmware, and the
    // cloud fixtures carry a live answer.
    for (name, fixture) in fixtures() {
        let sources = sources_of(&fixture);
        let with_record = HostSources {
            machine_type_record: Some(L4_RECORD.to_string()),
            ..sources.clone()
        };
        assert_eq!(
            identify(&with_record)
                .unwrap_or_else(|err| panic!("{name}: a record must not break detection: {err}")),
            identify(&sources).expect("detects"),
            "{name}"
        );
    }
}

#[test]
fn a_record_whose_facts_changed_is_refused_naming_the_fact() {
    let live = l4_live_sources();
    let cpuinfo = live.cpuinfo.clone().expect("cpuinfo");
    let meminfo = live.proc_meminfo.clone().expect("meminfo");
    let bus = live.pci_devices.clone().expect("pci");
    assert!(meminfo.contains("MemTotal:       32860372 kB"));
    assert!(bus.contains("0x10de 0x27b8"));

    let extra_cpus = (8..12)
        .map(|n| format!("processor\t: {n}\nvendor_id\t: GenuineIntel\n\n"))
        .collect::<Vec<_>>()
        .concat();
    for (label, sources, expected) in [
        (
            "twelve logical CPUs",
            HostSources {
                cpuinfo: Some(format!("{cpuinfo}{extra_cpus}")),
                ..l4_offline_sources(Some(L4_RECORD))
            },
            "logical CPU count was 8 when recorded and is 12 now",
        ),
        (
            "MemTotal one kB larger",
            HostSources {
                proc_meminfo: Some(meminfo.replace("32860372 kB", "32860373 kB")),
                ..l4_offline_sources(Some(L4_RECORD))
            },
            "MemTotal was 33649020928 bytes when recorded and is 33649021952 bytes now",
        ),
        (
            "a second L4",
            HostSources {
                pci_devices: Some(format!("{bus}\n0000:00:07.0 0x10de 0x27b8 0x030200")),
                ..l4_offline_sources(Some(L4_RECORD))
            },
            "NVIDIA display devices were [0x10de:0x27b8] when recorded and are \
             [0x10de:0x27b8, 0x10de:0x27b8] now",
        ),
        (
            "a different card",
            HostSources {
                pci_devices: Some(bus.replace("0x10de 0x27b8", "0x10de 0x2330")),
                ..l4_offline_sources(Some(L4_RECORD))
            },
            "NVIDIA display devices were [0x10de:0x27b8] when recorded and are \
             [0x10de:0x2330] now",
        ),
    ] {
        let detail = unestablished_detail(&sources);
        assert!(
            detail.contains(expected) && detail.contains("g2-standard-8"),
            "{label}: must name the fact, both values and the machine type: {detail}"
        );
    }
}

#[test]
fn memtotal_is_compared_exactly_across_the_two_recorded_l4_images() {
    // Both committed L4 recordings are g2-standard-8, on different images,
    // and their MemTotal differs by 8 kB. A record written on one does not
    // vouch for the other: exact equality refuses, and the next start with
    // the metadata service reachable records the new value.
    let (_, dlvm) = fixtures()
        .into_iter()
        .find(|(name, _)| name == "dlvm-ubuntu2404-l4-g2s8")
        .expect("the recorded DLVM fixture is committed");
    let on_dlvm = HostSources {
        dmi_product_name: Some("Google Compute Engine\n".to_string()),
        gce_machine_type: None,
        machine_type_record: Some(L4_RECORD.to_string()),
        boot_id: l4_live_sources().boot_id,
        ..sources_of(&dlvm)
    };
    let detail = unestablished_detail(&on_dlvm);
    assert!(
        detail
            .contains("MemTotal was 33649020928 bytes when recorded and is 33649029120 bytes now"),
        "{detail}"
    );
}

#[test]
fn a_record_this_release_cannot_use_is_refused_never_ignored() {
    for (label, record) in [
        (
            "an unknown field",
            L4_RECORD.replace(
                "\"schema_version\": 2,",
                "\"schema_version\": 2,\n  \"unexpected\": \"x\",",
            ),
        ),
        (
            "another schema version",
            L4_RECORD.replace("\"schema_version\": 2", "\"schema_version\": 1"),
        ),
        (
            "a machine type no row could name",
            L4_RECORD.replace("\"g2-standard-8\"", "\"G2 Standard 8\""),
        ),
        (
            "a missing fact",
            L4_RECORD.replace("  \"logical_cpus\": 8,\n", ""),
        ),
        ("not JSON", "g2-standard-8\n".to_string()),
    ] {
        let detail = unestablished_detail(&l4_offline_sources(Some(&record)));
        assert!(
            detail.contains("recorded machine type is unusable"),
            "{label}: {detail}"
        );
    }
}

#[test]
fn a_fact_that_cannot_be_read_now_never_counts_as_a_match() {
    for (label, sources, fact) in [
        (
            "no processor entries",
            HostSources {
                cpuinfo: Some("vendor_id\t: GenuineIntel\n".to_string()),
                ..l4_offline_sources(Some(L4_RECORD))
            },
            "logical CPU count",
        ),
        (
            "no MemTotal",
            HostSources {
                proc_meminfo: Some("MemFree:        28003732 kB\n".to_string()),
                ..l4_offline_sources(Some(L4_RECORD))
            },
            "MemTotal",
        ),
        (
            "no PCI bus",
            HostSources {
                pci_devices: None,
                ..l4_offline_sources(Some(L4_RECORD))
            },
            "NVIDIA display devices",
        ),
    ] {
        let detail = unestablished_detail(&sources);
        assert!(
            detail.contains("cannot be checked against this host") && detail.contains(fact),
            "{label}: {detail}"
        );
    }
}

#[test]
fn the_agent_records_exactly_what_detection_later_accepts() {
    let record = MachineTypeRecord::for_live_sources(&l4_live_sources())
        .expect("the facts are readable")
        .expect("a live answer on readable facts records");
    assert_eq!(
        record.to_json().expect("serializes"),
        L4_RECORD,
        "the bare machine type and the facts, never the project-scoped name"
    );
    assert_eq!(MachineTypeRecord::parse(L4_RECORD).expect("parses"), record);
    assert!(identify(&l4_offline_sources(Some(L4_RECORD))).is_ok());

    // Only a live answer is recorded.
    assert_eq!(
        MachineTypeRecord::for_live_sources(&l4_offline_sources(Some(L4_RECORD))),
        Ok(None)
    );
    let mut not_canonical = l4_live_sources();
    not_canonical.gce_machine_type =
        Some("projects/REDACTED/machineTypes/G2_STANDARD_8".to_string());
    assert_eq!(
        MachineTypeRecord::for_live_sources(&not_canonical),
        Ok(None)
    );
}

#[test]
fn a_live_answer_that_is_not_a_machine_type_resource_name_is_refused_and_never_recorded() {
    // The metadata service answers `projects/<project>/machineTypes/<type>`.
    // Anything else on a 200 -- a proxy's error page, a bare value -- is not
    // that answer. Taking whatever follows the last `/` would turn it into a
    // shape miss admitted as unvalidated, and recording it would persist the
    // same admission for every later offline start.
    for body in [
        "<html><body>proxy</body></html>",
        "Error: see https://example.invalid/errors/not-found",
        "g2-standard-8",
        "projects/REDACTED/machineTypes/G2_STANDARD_8",
        "projects//machineTypes/g2-standard-8",
        "projects/REDACTED/zones/g2-standard-8",
        "projects/REDACTED/machineTypes/g2-standard-8/extra",
        "projects/REDACTED/machineTypes/",
    ] {
        let sources = HostSources {
            gce_machine_type: Some(body.to_string()),
            machine_type_record: None,
            ..l4_live_sources()
        };
        match identify(&sources) {
            Err(PlatformProbeError::Unrecognized {
                source_name,
                detail,
            }) => {
                assert_eq!(source_name, "GCE metadata service", "{body}");
                assert!(!detail.contains("REDACTED"), "{body}: not echoed: {detail}");
                assert!(!detail.contains("proxy"), "{body}: not echoed: {detail}");
            }
            other => panic!("{body}: expected Unrecognized, got {other:?}"),
        }
        assert_eq!(
            MachineTypeRecord::for_live_sources(&sources),
            Ok(None),
            "{body}: never recorded"
        );
    }
}

#[test]
fn nothing_from_a_record_reaches_an_error_as_more_than_one_line() {
    // The agent prints detection errors to its journal, one line each. A
    // record is written by an account that is not root; whatever it holds
    // must not be able to print a line of its own.
    let forged = "x\nplatform admission: row=ubuntu2404-x86-l4-g2s8 evidence=validated";
    let escaped = serde_json::to_string(forged).expect("escapes");
    for (label, record) in [
        (
            "a machine type",
            L4_RECORD.replace("\"g2-standard-8\"", &escaped),
        ),
        (
            "a device entry",
            L4_RECORD.replace("\"0x10de:0x27b8\"", &escaped),
        ),
        (
            "an unknown key",
            L4_RECORD.replace(
                "\"schema_version\": 2,",
                &format!("\"schema_version\": 2,\n  {escaped}: 1,"),
            ),
        ),
    ] {
        let err = identify(&l4_offline_sources(Some(&record)))
            .expect_err(&format!("{label}: a forged record is refused"));
        let text = err.to_string();
        assert!(
            !text.contains('\n') && !text.contains("platform admission:"),
            "{label}: {text:?}"
        );
    }
}

#[test]
fn a_record_whose_device_entries_are_not_pci_ids_is_unusable() {
    for entry in [
        "0x10DE:0x27B8",
        "10de:27b8",
        "0x10de:0x27b",
        "0x10de:0x27b8 ",
    ] {
        let record = L4_RECORD.replace("\"0x10de:0x27b8\"", &format!("\"{entry}\""));
        let detail = match identify(&l4_offline_sources(Some(&record))) {
            Err(PlatformProbeError::IdentityUnestablished { detail, .. }) => detail,
            other => panic!("{entry}: expected IdentityUnestablished, got {other:?}"),
        };
        assert!(
            detail.contains("recorded machine type is unusable"),
            "{entry}: {detail}"
        );
    }
}
