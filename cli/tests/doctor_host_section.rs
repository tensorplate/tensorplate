// SPDX-License-Identifier: Apache-2.0
//
// `doctor` reports a host section describing the machine it is running on.
//
// The section exists so an operator can see what the device reports about
// itself before asking why a deploy was refused. What it must never do is
// describe the binary instead of the machine: the previous probe read
// `std::env::consts::ARCH`, which names the build target, so an `amd64`
// CLI on an arm64 host reported the wrong architecture with nothing to
// signal it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use tensorplate_cli::commands::doctor::finding::{Finding, FindingId, FindingStatus};
use tensorplate_platform::SystemHostProbe;

fn host_section() -> Vec<Finding> {
    tensorplate_cli::commands::doctor::host_section()
}

fn finding(id: FindingId) -> Finding {
    host_section()
        .into_iter()
        .find(|f| f.id == id)
        .unwrap_or_else(|| panic!("{} must be reported", id.as_str()))
}

#[test]
fn the_host_section_reports_facts_os_and_profile() {
    for id in [
        FindingId::HostFacts,
        FindingId::HostOs,
        FindingId::PlatformProfile,
    ] {
        let f = finding(id);
        assert!(!f.message.is_empty(), "{} must say something", id.as_str());
    }
}

#[test]
fn the_reported_architecture_is_the_machines_not_the_binarys() {
    // The regression this section exists to prevent. Asserted against the
    // platform probe rather than against `std::env::consts::ARCH`, which
    // is exactly the value that must not be used.
    let detected = SystemHostProbe::new()
        .detect()
        .expect("this host is detectable");
    let facts = finding(FindingId::HostFacts);
    assert_eq!(facts.status, FindingStatus::Pass);
    assert!(
        facts
            .message
            .contains(detected.identity.architecture.as_reported()),
        "host_facts must report the detected architecture: {}",
        facts.message
    );
    assert!(
        facts
            .message
            .contains(detected.identity.vendor.as_reported()),
        "host_facts must report the detected vendor: {}",
        facts.message
    );
}

#[test]
fn the_os_line_carries_the_exact_facts_matching_discards() {
    // Matching compares a normalized version; evidence needs the precise
    // one, and an operator filing a report should not have to run a second
    // command to get it.
    let detected = SystemHostProbe::new()
        .detect()
        .expect("this host is detectable");
    let os = finding(FindingId::HostOs);
    assert!(os.message.contains(&detected.identity.os_name));
    assert!(os.message.contains(&detected.identity.os_version));
    if let Some(exact) = detected.exact.os_version.as_deref() {
        assert!(
            os.message.contains(exact),
            "the exact version belongs in the operator-facing line: {}",
            os.message
        );
    }
}

#[test]
fn a_profile_that_cannot_be_matched_is_skipped_not_failed() {
    // On a host with no installed registry there is nothing to match
    // against. That is not this machine's fault and must not read as an
    // unsupported platform; the platform_registry finding owns that story.
    let profile = finding(FindingId::PlatformProfile);
    assert_ne!(
        profile.status,
        FindingStatus::Fail,
        "an unmatched profile is never a failure of doctor: {}",
        profile.message
    );
    if profile.status == FindingStatus::Skipped {
        assert!(
            profile
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("platform_registry"),
            "a skipped profile points at the finding that explains why"
        );
    }
}
