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
    identify, identify_jetson_accelerator, nvidia_pci_functions, CpuArchitecture, DetectedPlatform,
    HostSources, PlatformProbeError, PlatformReason, PlatformRegistry, RowMatch,
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
        gce_machine_type: text("gce_machine_type"),
        proc_meminfo: text("proc_meminfo"),
        pci_devices: text("pci_devices"),
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
    // This PR records the fact and gates nothing on it. If a later change
    // wires it into `HostIdentity`, this fails and whoever did it has to
    // say so deliberately.
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
