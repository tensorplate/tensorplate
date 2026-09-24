// SPDX-License-Identifier: Apache-2.0
//
// Why a Compute Engine instance has no live metadata answer, as detection
// reports it. The probe records the cause class in
// `HostSources::gce_metadata_unanswered`; when no same-boot record can stand
// in, the error names that class and what to do about it -- transient
// unavailability, blocked access, or not reached -- and when a record can,
// the class changes nothing.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use serde_json::Value;
use tensorplate_platform::{identify, HostSources, MachineTypeRecord, PlatformProbeError};

const BOOT: &str = "00000000-0000-4000-8000-000000000001";

/// The committed L4 host on the fixture boot, as a Compute Engine instance
/// with no live answer, for the reason `unanswered` names.
fn unanswered_l4(unanswered: Option<&str>, record: Option<String>) -> HostSources {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../test/platform/host_identity/ubuntu2404-x86-l4-g2s8.json");
    let fixture: Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("parses");
    let text = |key: &str| fixture["sources"][key].as_str().map(str::to_string);
    HostSources {
        uname_machine: text("uname_machine"),
        os_release: text("os_release"),
        cpuinfo: text("cpuinfo"),
        proc_meminfo: text("proc_meminfo"),
        pci_devices: text("pci_devices"),
        dmi_product_name: Some("Google Compute Engine\n".to_string()),
        boot_id: Some(format!("{BOOT}\n")),
        machine_type_record: record,
        gce_metadata_unanswered: unanswered.map(str::to_string),
        ..HostSources::default()
    }
}

#[test]
fn without_a_record_the_error_names_the_cause_class_and_its_remedy() {
    for (unanswered, class, remedy) in [
        (
            Some("http-503"),
            "answered HTTP 503 (transient unavailability",
            "it passes on its own",
        ),
        (
            Some("http-429"),
            "answered HTTP 429 (transient unavailability",
            "it passes on its own",
        ),
        (
            Some("refused"),
            "were refused (blocked access",
            "allow that address and port for tensorplate-agent",
        ),
        (
            Some("timeout"),
            "nothing answered from its metadata service within the budget (not reached",
            "a firewall rule, a proxy or custom routing drops the traffic",
        ),
        (
            None,
            "the metadata service could not be reached",
            "start tensorplate-agent once while the metadata service is reachable",
        ),
    ] {
        match identify(&unanswered_l4(unanswered, None)) {
            Err(PlatformProbeError::IdentityUnestablished { detail, .. }) => {
                assert!(detail.contains(class), "{unanswered:?}: {detail}");
                assert!(detail.contains(remedy), "{unanswered:?}: {detail}");
                assert!(
                    detail.contains("no machine type has been recorded on this host"),
                    "{unanswered:?}: the record's part of the story is kept: {detail}"
                );
            }
            other => panic!("{unanswered:?}: expected IdentityUnestablished, got {other:?}"),
        }
    }
}

#[test]
fn a_same_boot_record_stands_in_whatever_the_cause() {
    let live = HostSources {
        gce_machine_type: Some("projects/REDACTED/machineTypes/g2-standard-8".to_string()),
        machine_type_record: None,
        ..unanswered_l4(None, None)
    };
    let record = MachineTypeRecord::for_live_sources(&live)
        .expect("facts are readable")
        .expect("a live answer records")
        .to_json()
        .expect("serializes");
    for unanswered in ["http-503", "http-429", "refused", "timeout"] {
        let report = identify(&unanswered_l4(Some(unanswered), Some(record.clone())))
            .unwrap_or_else(|e| panic!("{unanswered}: {e}"));
        assert_eq!(
            report.identity.machine_type.as_deref(),
            Some("g2-standard-8"),
            "{unanswered}"
        );
    }
}
