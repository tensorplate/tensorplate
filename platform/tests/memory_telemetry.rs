// SPDX-License-Identifier: Apache-2.0
//
// Per-row memory telemetry over the committed fixtures.
//
// The property worth testing is not that numbers come back but that the
// memory MODEL is right per row: a discrete GPU's framebuffer is a second
// pool, and a unified-memory platform's is the same pool the OS lives in.
// Reporting the second as if it were the first tells an operator they
// have the whole of an Orin Nano's 8 GiB for a model.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use tempfile::TempDir;

use serde_json::Value;
use tensorplate_platform::row::GateValue;
use tensorplate_platform::{
    identify_accelerator, identify_platform, AcceleratorObservation, AcceleratorSources,
    HostSources, PlatformMemoryTelemetry, PlatformRegistry, PlatformReport,
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
    let body =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
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

fn report(host: &str, accelerator: Option<&str>) -> PlatformReport {
    let mut report = identify_platform(&sources(host)).expect("fixture detects");
    if let Some(name) = accelerator {
        let raw =
            std::fs::read_to_string(repo_path(&format!("test/platform/accelerator/{name}.txt")))
                .unwrap_or_else(|e| panic!("read accelerator fixture {name}: {e}"));
        let card = identify_accelerator(&AcceleratorSources {
            nvidia_smi_query: Some(raw),
        })
        .expect("interprets")
        .expect("one device");
        report.accelerator = Some(AcceleratorObservation {
            identity: card.identity,
            memory_bytes: card.exact.memory_total_bytes,
            memory_profile: PlatformMemoryProfileName::DiscreteGpu,
        });
    }
    report
}

fn telemetry(row_id: &str, host: &str, accelerator: Option<&str>) -> PlatformMemoryTelemetry {
    let registry = registry();
    let row = registry
        .row(row_id)
        .unwrap_or_else(|| panic!("{row_id} is committed"));
    PlatformMemoryTelemetry::collect(row, &report(host, accelerator))
        .unwrap_or_else(|| panic!("{row_id} declares an accelerator"))
}

#[test]
fn a_unified_memory_row_reports_one_pool_not_two() {
    // The Jetson's accelerator draws from the same memory the OS is in.
    // A caller that summed the two halves would double-count the only
    // pool the machine has.
    let jetson = telemetry(
        "jetson-orin-nano-8gb-jp62",
        "jetson-orin-nano-8gb-jp62",
        None,
    );
    assert_eq!(
        jetson.memory_profile(),
        PlatformMemoryProfileName::UnifiedMemory
    );
    assert!(jetson.shares_one_pool());
    assert_eq!(
        jetson.host_total_bytes(),
        jetson.accelerator_total_bytes(),
        "one pool means one number"
    );
}

#[test]
fn a_unified_row_falls_back_to_the_host_pool_when_the_probe_says_nothing() {
    // Detection normally fills a Jetson's accelerator figure from the
    // same meminfo the host figure comes from, so the two agree without
    // the fallback ever running. The fallback is for when it does not --
    // and on a one-pool machine the host figure IS the accelerator
    // figure, so reporting nothing would be strictly worse than
    // reporting the pool that is there.
    let registry = registry();
    let row = registry
        .row("jetson-orin-nano-8gb-jp62")
        .expect("committed");
    let mut probe_said_nothing = report("jetson-orin-nano-8gb-jp62", None);
    let host = probe_said_nothing.host.exact.host_total_memory_bytes;
    assert!(host.is_some(), "the fixture carries host memory");
    probe_said_nothing.accelerator = None;

    let telemetry = PlatformMemoryTelemetry::collect(row, &probe_said_nothing)
        .expect("the row declares an accelerator");
    assert_eq!(
        telemetry.accelerator_total_bytes(),
        host,
        "on one pool, an absent probe still leaves the pool the host reports"
    );
}

#[test]
fn an_apple_row_reports_one_pool_from_the_host_source() {
    // Same model, different source: Apple reports its unified pool
    // through `hw.memsize` rather than a device tree.
    let mac = telemetry("macos26-m1pro-16gb", "macos26-m1pro-16gb", None);
    assert!(mac.shares_one_pool());
    assert!(
        mac.host_total_bytes().is_some(),
        "the host pool must be read on macOS"
    );
    assert_eq!(mac.host_total_bytes(), mac.accelerator_total_bytes());
}

#[test]
fn a_discrete_row_keeps_the_framebuffer_separate_from_host_memory() {
    // Two pools. The recorded L4 answer reports 23034 MiB of framebuffer
    // on a host with its own memory, and conflating them would claim the
    // machine has one pool the size of the other.
    let l4 = telemetry(
        "ubuntu2404-x86-l4-g2s8",
        "ubuntu2404-x86-l4-g2s8",
        Some("ubuntu2404-x86-l4-g2s8"),
    );
    assert_eq!(l4.memory_profile(), PlatformMemoryProfileName::DiscreteGpu);
    assert!(!l4.shares_one_pool());
    let accelerator = l4
        .accelerator_total_bytes()
        .expect("the recorded answer carries a framebuffer");
    assert_ne!(
        Some(accelerator),
        l4.host_total_bytes(),
        "a discrete framebuffer is not the host pool"
    );
}

#[test]
fn a_missing_accelerator_figure_is_not_a_shortfall() {
    // The probe failing is not the machine being too small. A gate that
    // treated absence as a shortfall would refuse a machine for a probe
    // failure, which is a different fault with a different fix.
    let l4 = telemetry("ubuntu2404-x86-l4-g2s8", "ubuntu2404-x86-l4-g2s8", None);
    assert_eq!(l4.accelerator_total_bytes(), None);
    assert_eq!(l4.effective_budget_bytes(), None);
}

#[test]
fn the_memory_gate_is_a_row_fact_and_the_rows_agree_with_it() {
    // Every row that declares an accelerator gates memory as
    // load-bearing: a model that does not fit does not run, on any
    // chassis. This is the gate telemetry is collected for.
    let registry = registry();
    for row in registry.rows() {
        if row.accelerator().is_none() {
            continue;
        }
        assert_eq!(
            row.gate_semantics().memory.gate,
            GateValue::LoadBearing,
            "{}: memory is the one signal that gates everywhere",
            row.row_id()
        );
    }
}

#[test]
fn the_memory_gate_is_projected_without_reinterpreting_nominal_capacity() {
    // Every committed row gates memory as load-bearing, so the gate is
    // unfalsifiable against the shipped registry -- a check that ignored
    // it entirely would pass every test. This stages a row whose memory
    // gate is context-only and asserts that exact row fact reaches the
    // telemetry projection. Capacity remains a usable ceiling either way;
    // the gate applies to collector availability and model admission, not
    // to an invalid observed-versus-nominal equality test.
    let staged = stage_registry_with_context_only_memory("ubuntu2404-x86-l4-g2s8");
    let registry = PlatformRegistry::load(staged.path()).expect("staged registry loads");
    let row = registry.row("ubuntu2404-x86-l4-g2s8").expect("committed");

    let mut small = report("ubuntu2404-x86-l4-g2s8", Some("ubuntu2404-x86-l4-g2s8"));
    if let Some(accelerator) = small.accelerator.as_mut() {
        accelerator.memory_bytes = Some(4 * 1024 * 1024 * 1024);
    }
    let telemetry =
        PlatformMemoryTelemetry::collect(row, &small).expect("the row declares an accelerator");
    assert_eq!(telemetry.memory_gate(), GateValue::ContextOnly);
    assert_eq!(
        telemetry.effective_budget_bytes(),
        Some(4 * 1024 * 1024 * 1024)
    );
    // `staged` removes itself when it drops.
}

/// Copy the committed registry, weakening one row's memory gate.
///
/// Staged in a `TempDir` rather than under a name built from the pid: a
/// predictable path in the shared temp directory is one another local
/// user can pre-create as a symlink, and this writes a directory tree
/// through it. These tests run on shared lab hosts and self-hosted
/// runners, where that user exists.
fn stage_registry_with_context_only_memory(row_id: &str) -> TempDir {
    let staging = TempDir::new().expect("staging dir");
    copy_dir(&repo_path("config/platform"), staging.path());
    let path = staging.path().join(format!("rows/{row_id}.json"));
    let body = std::fs::read_to_string(&path).expect("read staged row");
    let mut row: Value = serde_json::from_str(&body).expect("row parses");
    row["gate_semantics"]["memory"] = serde_json::json!({
        "gate": "context_only"
    });
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&row).expect("serialize"),
    )
    .expect("write staged row");
    staging
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("create dir");
    for entry in std::fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("dir entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

#[test]
fn canonical_supported_hardware_uses_observed_memory_as_a_bounded_ceiling() {
    let cases = [
        (
            "jetson-orin-nano-8gb-jp62",
            "jetson-orin-nano-8gb-jp62",
            None,
        ),
        ("macos26-m1pro-16gb", "macos26-m1pro-16gb", None),
        (
            "ubuntu2404-x86-l4-g2s8",
            "ubuntu2404-x86-l4-g2s8",
            Some("ubuntu2404-x86-l4-g2s8"),
        ),
        (
            "ubuntu2404-x86-rtxpro6000se-g4s48",
            "ubuntu2404-x86-rtxpro6000se-g4s48",
            Some("ubuntu2404-x86-rtxpro6000se-g4s48"),
        ),
    ];

    for (row_id, host, accelerator) in cases {
        let telemetry = telemetry(row_id, host, accelerator);
        let observed = telemetry
            .accelerator_total_bytes()
            .unwrap_or_else(|| panic!("{row_id}: canonical fixture reports usable capacity"));
        assert_eq!(
            telemetry.effective_budget_bytes(),
            Some(observed.min(telemetry.row_nominal_capacity_bytes())),
            "{row_id}: usable capacity is bounded, not compared for nominal equality"
        );
    }

    for (row_id, host, accelerator) in [
        (
            "jetson-orin-nano-8gb-jp62",
            "jetson-orin-nano-8gb-jp62",
            None,
        ),
        (
            "ubuntu2404-x86-l4-g2s8",
            "ubuntu2404-x86-l4-g2s8",
            Some("ubuntu2404-x86-l4-g2s8"),
        ),
    ] {
        let telemetry = telemetry(row_id, host, accelerator);
        assert!(
            telemetry.accelerator_total_bytes().expect("observed")
                < telemetry.row_nominal_capacity_bytes(),
            "{row_id}: fixture must preserve the real nominal/usable distinction"
        );
        assert_eq!(
            telemetry.effective_budget_bytes(),
            telemetry.accelerator_total_bytes(),
            "{row_id}: a reservation is not a hardware shortfall"
        );
    }
}
