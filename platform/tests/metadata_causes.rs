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
            "answered HTTP 503 (transient unavailability: Google documents 503 while the \
             metadata server boots",
            "if it persists, something other than the metadata server may be answering",
        ),
        (
            Some("http-429"),
            "answered HTTP 429 (transient unavailability: Google documents 429 as an endpoint's \
             rate limiting",
            "if it persists, something other than the metadata server may be answering",
        ),
        (
            Some("refused"),
            "were refused (blocked access",
            "allow that address and port for tensorplate-agent",
        ),
        (
            Some("timeout"),
            "its metadata service was not reached: the connect failed or nothing answered within \
             the budget (not reached",
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

/// The record a live answer on the committed L4 host writes.
fn l4_record() -> String {
    let live = HostSources {
        gce_machine_type: Some("projects/REDACTED/machineTypes/g2-standard-8".to_string()),
        machine_type_record: None,
        ..unanswered_l4(None, None)
    };
    MachineTypeRecord::for_live_sources(&live)
        .expect("facts are readable")
        .expect("a live answer records")
        .to_json()
        .expect("serializes")
}

#[test]
fn with_a_record_that_cannot_stand_in_the_error_still_opens_with_the_cause_class() {
    let record = l4_record();
    let mut other_shape: Value = serde_json::from_str(&record).expect("parses");
    other_shape["logical_cpus"] = (other_shape["logical_cpus"].as_u64().expect("count") + 1).into();
    let refused = |record: String| unanswered_l4(Some("refused"), Some(record));
    for (label, sources, reason) in [
        (
            "unusable",
            refused("not a record".to_string()),
            "the recorded machine type is unusable",
        ),
        (
            "unchecked",
            HostSources {
                boot_id: None,
                ..refused(record.clone())
            },
            "cannot be checked against this host",
        ),
        (
            "stale",
            refused(other_shape.to_string()),
            "no longer describes this host",
        ),
        (
            "unbound",
            HostSources {
                instance_binding: Some("not a binding".to_string()),
                ..refused(record.clone())
            },
            "the instance binding is unusable",
        ),
    ] {
        match identify(&sources) {
            Err(PlatformProbeError::IdentityUnestablished { detail, .. }) => {
                assert!(
                    detail.starts_with(
                        "host reports as a Compute Engine instance and connections to its \
                         metadata service at 169.254.169.254:80 were refused (blocked access"
                    ),
                    "{label}: {detail}"
                );
                assert!(detail.contains(reason), "{label}: {detail}");
            }
            other => panic!("{label}: expected IdentityUnestablished, got {other:?}"),
        }
    }
}

#[test]
fn a_binding_on_another_machine_type_is_refused_naming_the_cause_class() {
    // Offline, an earlier-boot binding on another machine type than this
    // boot's record: the remedy is an online start, so the message names the
    // cause class of the missing answer too, after the refusal itself.
    let binding = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/identity/instance-binding-v1.json"),
    )
    .expect("binding fixture")
    .replace("g2-standard-8", "g2-standard-4")
    .replace(BOOT, "00000000-0000-4000-8000-000000000002");
    let sources = HostSources {
        instance_binding: Some(binding),
        ..unanswered_l4(Some("refused"), Some(l4_record()))
    };
    match identify(&sources) {
        Err(PlatformProbeError::MachineTypeChanged { detail, .. }) => {
            assert!(
                detail.starts_with("the machine type recorded in this boot is `g2-standard-8`"),
                "the refusal leads: {detail}"
            );
            assert!(
                detail.contains(
                    "(host reports as a Compute Engine instance and connections to its \
                     metadata service at 169.254.169.254:80 were refused (blocked access"
                ),
                "the cause class follows: {detail}"
            );
            assert!(detail.contains("`g2-standard-4`"), "{detail}");
        }
        other => panic!("expected MachineTypeChanged, got {other:?}"),
    }
}

#[test]
fn a_same_boot_record_stands_in_whatever_the_cause() {
    let record = l4_record();
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
