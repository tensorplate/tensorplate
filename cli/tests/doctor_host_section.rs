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
use tensorplate_cli::commands::doctor::render_host_section;
use tensorplate_platform::{
    identify, HostSources, PlatformProbeError, PlatformRegistry, PlatformRegistryError,
};

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
    }
}

fn fixture(name: &str) -> Value {
    let path = repo_path(&format!("test/platform/host_identity/{name}.json"));
    let body =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&body).expect("fixture parses")
}

/// Render the host section for one committed row's host identity.
fn section_for(name: &str) -> Vec<Finding> {
    let report = identify(&sources_of(&fixture(name))).expect("fixture detects");
    let registry = registry();
    render_host_section(Ok(&report), Ok(&registry))
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

/// Every Production row, plus the two CPU-only Preview rows, so the
/// operator-facing output for each is reviewed as a diff.
const GOLDEN_ROWS: [&str; 7] = [
    "jetson-orin-nano-8gb-jp62",
    "macos26-m1pro-16gb",
    "ubuntu2404-x86-a100-40g-a2hg1",
    "ubuntu2404-x86-l4-g2s8",
    "ubuntu2404-x86-rtxpro6000se-g4s48",
    "ubuntu2204-x86-cpu",
    "ubuntu2404-x86-cpu",
];

#[test]
fn the_host_section_for_every_supported_row_matches_its_golden() {
    let mut rendered = String::new();
    for name in GOLDEN_ROWS {
        rendered.push_str(&format!("## {name}\n"));
        rendered.push_str(&render(&section_for(name)));
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
    for name in GOLDEN_ROWS {
        let section = section_for(name);
        let profile = section
            .iter()
            .find(|f| f.id == FindingId::PlatformProfile)
            .expect("a profile finding");
        assert_eq!(
            profile.status,
            FindingStatus::Pass,
            "{name}: a committed row's own host must match something"
        );
        assert!(
            profile.message.contains(name),
            "{name}: its own row must be among the candidates: {}",
            profile.message
        );
    }
}

#[test]
fn an_off_matrix_host_renders_a_typed_no_match_not_a_failure() {
    let mut riscv = sources_of(&fixture("ubuntu2404-x86-cpu"));
    riscv.uname_machine = Some("riscv64".to_string());
    let report = identify(&riscv).expect("detects");
    let registry = registry();
    let section = render_host_section(Ok(&report), Ok(&registry));

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
    let section = render_host_section(Err(&err), Err(&NO_REGISTRY));
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
}

#[test]
fn a_missing_registry_skips_the_profile_without_touching_the_host_lines() {
    let report = identify(&sources_of(&fixture("macos26-m1pro-16gb"))).expect("detects");
    let section = render_host_section(Ok(&report), Err(&NO_REGISTRY));

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
    let report = identify(&on_unknown_shape).expect("detects");

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

    let section = render_host_section(Ok(&report), Ok(&registry));
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
