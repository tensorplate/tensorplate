// SPDX-License-Identifier: Apache-2.0
//
// `doctor`'s host section, rendered from the committed host-identity
// fixtures.
//
// Driven from fixtures rather than from the machine running the test, for
// two reasons. It is the only way to assert what an operator sees on a
// Jetson or a G2 instance from a laptop; and a live probe would make the
// suite depend on the reviewer's host — a sandboxed macOS where `sysctl`
// returns `Operation not permitted` would fail tests that have nothing to
// do with the change under review.
//
// Regenerate with:
//   UPDATE_GOLDEN=1 cargo test -p tensorplate-cli --test doctor_host_section

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use serde_json::Value;
use tensorplate_cli::commands::doctor::finding::{Finding, FindingId, FindingStatus};
use tensorplate_cli::commands::doctor::{render_host_section, HostSectionDetection};
use tensorplate_platform::{
    identify_accelerator, identify_platform, AcceleratorObservation, AcceleratorSources,
    HostSources, PlatformProbeError, PlatformRegistry, PlatformRegistryError, PlatformReport,
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

fn fixture(name: &str) -> Value {
    let path = repo_path(&format!("test/platform/host_identity/{name}.json"));
    let body =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&body).expect("fixture parses")
}

/// Detect a fixture the way `doctor` does on a real host: host sources
/// first, then the discrete card when the host sources named none. The
/// accelerator fixture is optional -- a CPU-only row has none, and that
/// absence is itself what the row match must handle.
fn report_for(name: &str, accelerator: Option<&str>) -> PlatformReport {
    let mut report = identify_platform(&sources_of(&fixture(name))).expect("fixture detects");
    if report.accelerator.is_none() {
        if let Some(accelerator) = accelerator {
            let raw = std::fs::read_to_string(repo_path(&format!(
                "test/platform/accelerator/{accelerator}.txt"
            )))
            .unwrap_or_else(|e| panic!("read accelerator fixture {accelerator}: {e}"));
            let card = identify_accelerator(&AcceleratorSources {
                nvidia_smi_query: Some(raw),
            })
            .expect("accelerator fixture interprets")
            .expect("accelerator fixture carries a device");
            report.accelerator = Some(AcceleratorObservation {
                identity: card.identity,
                memory_bytes: card.exact.memory_total_bytes,
                memory_profile: PlatformMemoryProfileName::DiscreteGpu,
            });
        }
    }
    report
}

/// Render the host section for one committed row's identity.
fn section_for(name: &str, accelerator: Option<&str>) -> Vec<Finding> {
    let registry = registry();
    let report = report_for(name, accelerator);
    render_host_section(HostSectionDetection::Complete(&report), Ok(&registry))
}

/// A stand-in load failure for tests that do not care which one it was.
static NO_REGISTRY: PlatformRegistryError = PlatformRegistryError::AmbiguousRegistry {
    detail: String::new(),
};

fn render(findings: &[Finding]) -> String {
    findings
        .iter()
        .map(|f| {
            let hint = f
                .hint
                .as_deref()
                .map_or(String::new(), |h| format!("\n    hint: {h}"));
            format!(
                "[{}] {} {} — {}{hint}",
                f.status_label(),
                f.severity_label(),
                f.id_label(),
                f.message
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every supported row and a representative fixture for it, so the
/// operator-facing output for each is reviewed as a diff. Family rows use a
/// member SKU because their canonical display identity is not a detected SKU.
/// Every supported row, its host fixture, and the accelerator fixture a
/// real host of that row would report. `None` is a row whose accelerator
/// comes from the host sources themselves (Apple, Jetson) or which has
/// none at all (CPU-only) -- both are cases the row match must handle.
const GOLDEN_ROWS: [(&str, &str, Option<&str>); 8] = [
    (
        "jetson-orin-nano-8gb-jp62",
        "jetson-orin-nano-8gb-jp62",
        None,
    ),
    ("macos26-apple-m-series-preview", "macos26-m2pro-16gb", None),
    ("macos26-m1pro-16gb", "macos26-m1pro-16gb", None),
    (
        "ubuntu2404-x86-a100-40g-a2hg1",
        "ubuntu2404-x86-a100-40g-a2hg1",
        Some("ubuntu2404-x86-a100-40g-a2hg1"),
    ),
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
    ("ubuntu2204-x86-cpu", "ubuntu2204-x86-cpu", None),
    ("ubuntu2404-x86-cpu", "ubuntu2404-x86-cpu", None),
];

#[test]
fn the_host_section_for_every_supported_row_matches_its_golden() {
    let mut rendered = String::new();
    for (row_id, fixture_name, accelerator) in GOLDEN_ROWS {
        rendered.push_str(&format!("## {row_id}\n"));
        rendered.push_str(&render(&section_for(fixture_name, accelerator)));
        rendered.push_str("\n\n");
    }

    let path = repo_path("test/platform/doctor_host_section.golden.txt");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &rendered).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{} is missing ({e}); regenerate with UPDATE_GOLDEN=1",
            path.display()
        )
    });
    assert_eq!(
        rendered, expected,
        "host section output changed — regenerate with `UPDATE_GOLDEN=1 cargo test -p tensorplate-cli --test doctor_host_section`"
    );
}

#[test]
fn a_supported_row_is_named_as_a_candidate() {
    // The claim that matters per row: the machine the row describes sees
    // its own row offered.
    for (row_id, fixture_name, accelerator) in GOLDEN_ROWS {
        let section = section_for(fixture_name, accelerator);
        let profile = section
            .iter()
            .find(|f| f.id == FindingId::PlatformProfile)
            .expect("a profile finding");
        assert_eq!(
            profile.status,
            FindingStatus::Pass,
            "{row_id}: a committed row's representative host must match something"
        );
        assert!(
            profile.message.contains(row_id),
            "{row_id}: its own row must be among the candidates: {}",
            profile.message
        );
    }
}

#[test]
fn every_production_row_resolves_to_its_exact_row_id() {
    // The claim `platform_profile` cannot make and defers: not "could be
    // one of these" but "is this one". Asserted per row rather than as a
    // golden diff, because a golden proves the text did not change while
    // this proves the text is right.
    for (row_id, fixture_name, accelerator) in GOLDEN_ROWS {
        let section = section_for(fixture_name, accelerator);
        let row = section
            .iter()
            .find(|f| f.id == FindingId::PlatformRow)
            .expect("a row finding");
        assert!(
            row.message.contains(row_id),
            "{row_id}: must resolve to its own row, got {}",
            row.message
        );
    }
}

#[test]
fn a_driverless_gpu_host_is_not_reported_as_a_cpu_row() {
    // The silent-downgrade case. The host has an NVIDIA card on the PCI
    // bus and no working driver, so no accelerator is identified --
    // resolving on host identity alone lands it on the CPU-only row and
    // tells the operator their broken machine is supported. Deploy
    // admission refuses this machine; doctor must not disagree.
    let report = report_for("ubuntu2404-x86-l4-g2s8", None);
    assert!(
        !report.host.exact.nvidia_pci_functions.is_empty(),
        "the fixture must carry PCI evidence of a GPU"
    );
    assert!(
        report.accelerator.is_none(),
        "and no identified accelerator"
    );

    let registry = registry();
    let section = render_host_section(HostSectionDetection::Complete(&report), Ok(&registry));
    let row = section
        .iter()
        .find(|f| f.id == FindingId::PlatformRow)
        .expect("a row finding");
    assert_eq!(row.status, FindingStatus::Unsupported);
    assert!(
        row.message.contains("missing_driver_runtime"),
        "the typed reason must say the driver is the problem: {}",
        row.message
    );
    assert!(
        !row.message.contains("ubuntu2404-x86-cpu"),
        "a broken GPU host must never read as a supported CPU box: {}",
        row.message
    );
    let model_classes = section
        .iter()
        .find(|f| f.id == FindingId::ModelClassRows)
        .expect("a model-class finding");
    assert_eq!(model_classes.status, FindingStatus::Skipped);
    assert!(
        !model_classes.message.contains("ubuntu2404-x86-cpu"),
        "dependent findings must consume the same resolution: {}",
        model_classes.message
    );
}

#[test]
fn an_unreadable_accelerator_with_pci_evidence_names_the_driver_failure() {
    let report = report_for("ubuntu2404-x86-l4-g2s8", None);
    let error = PlatformProbeError::Unreadable {
        source_name: "nvidia-smi".to_string(),
        detail: "driver communication failed".to_string(),
    };
    let registry = registry();

    let section = render_host_section(
        HostSectionDetection::AcceleratorProbeFailed {
            host: &report.host,
            error: &error,
        },
        Ok(&registry),
    );
    let row = section
        .iter()
        .find(|f| f.id == FindingId::PlatformRow)
        .expect("a row finding");
    assert_eq!(row.status, FindingStatus::Unsupported);
    assert!(row.message.contains("missing_driver_runtime"));
    assert!(row.message.contains("driver communication failed"));
    assert_eq!(
        section
            .iter()
            .find(|f| f.id == FindingId::ModelClassRows)
            .expect("a model-class finding")
            .status,
        FindingStatus::Skipped,
    );
}

#[test]
fn a_multi_gpu_answer_names_its_topology_rather_than_failing_detection() {
    // This used to render "accelerator detection failed" as a Warning,
    // because two cards produced a probe error. Two readable cards are an
    // answer, so the host now resolves normally and is refused with the
    // fact an operator can act on. The difference matters at the console:
    // a Warning saying detection failed invites them to debug their
    // driver or their nvidia-smi; an Unsupported row naming the topology
    // tells them this release serves one device.
    let mut report = report_for("ubuntu2404-x86-l4-g2s8", Some("ubuntu2404-x86-l4-g2s8"));
    report
        .accelerator
        .as_mut()
        .expect("the fixture carries an accelerator")
        .identity
        .device_count = 2;
    let registry = registry();

    let section = render_host_section(HostSectionDetection::Complete(&report), Ok(&registry));

    let facts = section
        .iter()
        .find(|f| f.id == FindingId::HostFacts)
        .expect("host facts");
    assert_eq!(facts.status, FindingStatus::Pass);
    let row = section
        .iter()
        .find(|f| f.id == FindingId::PlatformRow)
        .expect("a row finding");
    assert_eq!(
        row.status,
        FindingStatus::Unsupported,
        "a machine no row claims is unsupported, not a warning: {row:?}"
    );
    assert!(
        row.message.contains("unsupported_accelerator_topology"),
        "the reason must name the topology: {}",
        row.message
    );
    assert!(
        !row.message.contains("missing_driver_runtime"),
        "the driver answered and is not at fault: {}",
        row.message
    );
    assert!(
        !row.message.contains("detection failed"),
        "nothing failed to detect: {}",
        row.message
    );

    // The generic "see the support matrix" pointer is actively unhelpful
    // here: this card IS on the matrix. The hint has to say that the
    // count is what was refused.
    let hint = row
        .hint
        .as_deref()
        .expect("an unsupported row carries a hint");
    assert!(
        hint.contains("one accelerator per host"),
        "the hint must name the actual constraint: {hint}"
    );
    assert!(
        !hint.contains("support-matrix.md"),
        "pointing at the matrix would tell them their supported card is supported: {hint}"
    );
}

#[test]
fn malformed_integrated_accelerator_facts_do_not_erase_the_host() {
    let mut sources = sources_of(&fixture("macos26-m1pro-16gb"));
    sources.hw_memsize = Some("sixteen gibibytes".to_string());
    let host = tensorplate_platform::identify(&sources).expect("host still interprets");
    let error = identify_platform(&sources).expect_err("accelerator memory must be numeric");
    let registry = registry();

    let section = render_host_section(
        HostSectionDetection::AcceleratorProbeFailed {
            host: &host,
            error: &error,
        },
        Ok(&registry),
    );
    for id in [
        FindingId::HostFacts,
        FindingId::HostOs,
        FindingId::PlatformProfile,
    ] {
        let finding = section
            .iter()
            .find(|finding| finding.id == id)
            .expect("host-derived finding");
        assert_eq!(
            finding.status,
            FindingStatus::Pass,
            "{id:?} must survive an accelerator-only error"
        );
    }
    let row = section
        .iter()
        .find(|f| f.id == FindingId::PlatformRow)
        .expect("a row finding");
    assert_eq!(row.status, FindingStatus::Warning);
    assert!(row.message.contains("hw.memsize"));
    assert!(!row.message.contains("missing_driver_runtime"));
    assert_eq!(
        section
            .iter()
            .find(|f| f.id == FindingId::ModelClassRows)
            .expect("a model-class finding")
            .status,
        FindingStatus::Skipped,
    );
}

#[test]
fn the_model_classes_come_from_the_registry_not_a_list_here() {
    // The posture each row actually claims, read from its pointers. The
    // shapes differ per row and that is the point: the Jetson carries the
    // one Production policy class, the G4 carries all three VLA shapes,
    // and the deploy-smoke rows carry Preview. A hardcoded list would
    // drift the first time a row changed.
    for (row_id, fixture_name, accelerator, expected) in [
        (
            "jetson-orin-nano-8gb-jp62",
            "jetson-orin-nano-8gb-jp62",
            None,
            vec!["chunked_policy (Production)"],
        ),
        (
            "ubuntu2404-x86-rtxpro6000se-g4s48",
            "ubuntu2404-x86-rtxpro6000se-g4s48",
            Some("ubuntu2404-x86-rtxpro6000se-g4s48"),
            vec![
                "chunked_policy (Production)",
                "autoregressive_action_tokens (Production)",
                "flow_action_chunk (Production)",
            ],
        ),
        (
            "ubuntu2404-x86-l4-g2s8",
            "ubuntu2404-x86-l4-g2s8",
            Some("ubuntu2404-x86-l4-g2s8"),
            vec!["chunked_policy (Preview)"],
        ),
        (
            "macos26-m1pro-16gb",
            "macos26-m1pro-16gb",
            None,
            vec!["chunked_policy (Preview)"],
        ),
    ] {
        let section = section_for(fixture_name, accelerator);
        let finding = section
            .iter()
            .find(|f| f.id == FindingId::ModelClassRows)
            .expect("a model-class finding");
        assert_eq!(finding.status, FindingStatus::Pass, "{row_id}");
        for class in expected {
            assert!(
                finding.message.contains(class),
                "{row_id}: must name `{class}` from the registry: {}",
                finding.message
            );
        }
    }
}

#[test]
fn a_row_claiming_no_model_classes_says_so_rather_than_rendering_empty() {
    // The A100 row is Planned, and the registry refuses to let a Planned
    // row carry model-class claims. An empty render would read as a
    // broken row rather than an honest one.
    let section = section_for(
        "ubuntu2404-x86-a100-40g-a2hg1",
        Some("ubuntu2404-x86-a100-40g-a2hg1"),
    );
    let finding = section
        .iter()
        .find(|f| f.id == FindingId::ModelClassRows)
        .expect("a model-class finding");
    assert!(
        finding.message.contains("claims no model classes"),
        "got {}",
        finding.message
    );
}

#[test]
fn model_classes_are_skipped_when_no_row_matched() {
    // Nothing to explain when nothing resolved, and the row finding
    // already carries the reason -- repeating it here would give an
    // operator two things to read for one fact.
    let mut riscv = sources_of(&fixture("ubuntu2404-x86-cpu"));
    riscv.uname_machine = Some("riscv64".to_string());
    let report = identify_platform(&riscv).expect("detects");
    let registry = registry();
    let section = render_host_section(HostSectionDetection::Complete(&report), Ok(&registry));
    let finding = section
        .iter()
        .find(|f| f.id == FindingId::ModelClassRows)
        .expect("a model-class finding");
    assert_eq!(finding.status, FindingStatus::Skipped);
}

#[test]
fn a_near_miss_os_version_resolves_to_no_row() {
    // One dimension wrong, everything else exact. Matching is exact
    // string equality, so an OS the matrix does not name must not be
    // absorbed by the row it otherwise looks like.
    let mut sources = sources_of(&fixture("ubuntu2404-x86-l4-g2s8"));
    sources.os_release = Some(
        sources
            .os_release
            .expect("the fixture carries os-release")
            .replace("24.04", "23.10"),
    );
    let report = identify_platform(&sources).expect("detects");
    let registry = registry();
    let section = render_host_section(HostSectionDetection::Complete(&report), Ok(&registry));

    let row = section
        .iter()
        .find(|f| f.id == FindingId::PlatformRow)
        .expect("a row finding");
    assert_eq!(row.status, FindingStatus::Unsupported);
    assert!(
        !row.message.contains("ubuntu2404-x86-l4-g2s8"),
        "a 23.10 host must not resolve to the 24.04 row: {}",
        row.message
    );
}

#[test]
fn a_near_miss_accelerator_sku_resolves_to_no_row() {
    // The other half of the identity, and the one `platform_profile`
    // structurally cannot catch: the host profile is a perfect match for
    // the L4 row, and only the card is wrong.
    let mut report = report_for("ubuntu2404-x86-l4-g2s8", None);
    let raw = std::fs::read_to_string(repo_path(
        "test/platform/accelerator/unsupported-rtx-a6000.txt",
    ))
    .expect("an off-matrix accelerator fixture");
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

    let registry = registry();
    let section = render_host_section(HostSectionDetection::Complete(&report), Ok(&registry));

    let profile = section
        .iter()
        .find(|f| f.id == FindingId::PlatformProfile)
        .expect("a profile finding");
    assert_eq!(
        profile.status,
        FindingStatus::Pass,
        "the host half is a genuine match; only the card is off-matrix"
    );

    let row_finding = section
        .iter()
        .find(|f| f.id == FindingId::PlatformRow)
        .expect("a row finding");
    assert_eq!(row_finding.status, FindingStatus::Unsupported);
    assert!(
        row_finding.message.contains("unsupported_accelerator_sku"),
        "the typed reason must name the dimension that missed: {}",
        row_finding.message
    );
}

#[test]
fn an_off_matrix_host_renders_a_typed_no_match_not_a_failure() {
    let mut riscv = sources_of(&fixture("ubuntu2404-x86-cpu"));
    riscv.uname_machine = Some("riscv64".to_string());
    let report = identify_platform(&riscv).expect("detects");
    let registry = registry();
    let section = render_host_section(HostSectionDetection::Complete(&report), Ok(&registry));

    let profile = section
        .iter()
        .find(|f| f.id == FindingId::PlatformProfile)
        .expect("a profile finding");
    assert_eq!(
        profile.status,
        FindingStatus::Unsupported,
        "an off-matrix host is unsupported, not a doctor failure"
    );
    assert_ne!(profile.status, FindingStatus::Fail);
    assert!(
        profile.message.contains("unsupported_cpu_arch"),
        "the typed reason reaches the operator: {}",
        profile.message
    );
}

#[test]
fn undetectable_host_identity_never_fails_doctor() {
    // The regression that would otherwise land on a sandboxed host: a
    // blocked `sysctl` must not flip doctor's exit code. `doctor` is what
    // an operator runs to diagnose exactly that.
    let err = PlatformProbeError::Unreadable {
        source_name: "sysctl".to_string(),
        detail: "Operation not permitted".to_string(),
    };
    let section = render_host_section(
        HostSectionDetection::HostProbeFailed(&err),
        Err(&NO_REGISTRY),
    );
    assert!(
        section.iter().all(|f| f.status != FindingStatus::Fail),
        "undetected identity must not produce a failing finding: {}",
        render(&section)
    );
    let facts = section
        .iter()
        .find(|f| f.id == FindingId::HostFacts)
        .expect("host_facts");
    assert_eq!(facts.status, FindingStatus::Warning);
    assert!(facts.message.contains("Operation not permitted"));
    assert_eq!(section.len(), 5, "every host-section finding ID is stable");
    for id in [
        FindingId::HostFacts,
        FindingId::HostOs,
        FindingId::PlatformProfile,
        FindingId::PlatformRow,
        FindingId::ModelClassRows,
    ] {
        assert_eq!(
            section.iter().filter(|finding| finding.id == id).count(),
            1,
            "{id:?} must be emitted exactly once"
        );
    }
}

#[test]
fn a_missing_registry_skips_the_profile_without_touching_the_host_lines() {
    let report = identify_platform(&sources_of(&fixture("macos26-m1pro-16gb"))).expect("detects");
    let section = render_host_section(HostSectionDetection::Complete(&report), Err(&NO_REGISTRY));

    let profile = section
        .iter()
        .find(|f| f.id == FindingId::PlatformProfile)
        .expect("a profile finding");
    assert_eq!(profile.status, FindingStatus::Skipped);
    assert!(profile
        .hint
        .as_deref()
        .unwrap_or_default()
        .contains("platform_registry"));
    // Must not assert the registry is absent: it may be installed and
    // merely unreadable, and "reinstall it" is then the wrong next step.
    assert!(
        !profile.message.contains("no platform registry"),
        "the profile must not claim absence it cannot know: {}",
        profile.message
    );

    // The host facts are still reported: they do not depend on a registry.
    let facts = section
        .iter()
        .find(|f| f.id == FindingId::HostFacts)
        .expect("host_facts");
    assert_eq!(facts.status, FindingStatus::Pass);
    assert!(facts.message.contains("apple"));
}

#[test]
fn an_environment_only_miss_does_not_blame_the_os() {
    // A host whose hardware this release validates but whose machine shape
    // no row covers has a perfectly supported OS. Telling its operator the
    // OS version is unsupported sends them to reinstall the wrong thing.
    let mut on_unknown_shape = sources_of(&fixture("ubuntu2404-x86-l4-g2s8"));
    on_unknown_shape.gce_machine_type = Some("projects/1/machineTypes/g2-standard-16".to_string());
    let report = identify_platform(&on_unknown_shape).expect("detects");

    // Against a registry of shape-scoped rows only, so the chassis-
    // independent CPU row cannot absorb the host.
    let scoped = ["ubuntu2404-x86-l4-g2s8", "ubuntu2404-x86-a100-40g-a2hg1"].map(|name| {
        let path = repo_path(&format!("config/platform/rows/{name}.json"));
        (path.clone(), std::fs::read_to_string(&path).expect("read"))
    });
    let registry = PlatformRegistry::from_documents(
        scoped.iter().map(|(p, b)| (p.as_path(), b.as_str())),
        std::iter::empty(),
    )
    .expect("loads");

    let section = render_host_section(HostSectionDetection::Complete(&report), Ok(&registry));
    let profile = section
        .iter()
        .find(|f| f.id == FindingId::PlatformProfile)
        .expect("a profile finding");
    assert_eq!(profile.status, FindingStatus::Unsupported);
    assert!(
        !profile.message.contains("unsupported_os_version"),
        "the OS is supported; only the machine shape is not: {}",
        profile.message
    );
    assert!(
        profile.message.contains("machine shape"),
        "the message names what is actually wrong: {}",
        profile.message
    );
}
