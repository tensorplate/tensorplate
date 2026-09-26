// SPDX-License-Identifier: Apache-2.0
//
// The machine-type record a rollback hands back to the 0.2.1 agent, and the
// instance binding beside it.
//
// `fixtures/identity/machine-type-record-v2.json` is what this release's
// writer produces for the committed L4 host fixture on a synthetic boot, and
// the record's layout, parser and serializer are the ones v0.2.1 shipped (the
// file I/O around them is shared with the binding now). So the byte
// comparison below is the compatibility claim: the record a rollback
// restores is one the predecessor reads, and one this release still writes
// the same way. `instance-binding-v1.json` binds that record to a synthetic
// instance id.
//
// Detection is driven through `identify` with the L4 host fixture's sources,
// online (both metadata answers present) and offline (neither).

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use serde_json::{json, Value};
use tensorplate_platform::instance_binding::sha256_hex;
use tensorplate_platform::{
    identify, HostSources, InstanceBinding, MachineTypeRecord, MachineTypeSource,
    PlatformProbeError,
};
use tensorplate_protocol::install_paths::{INSTANCE_BINDING_PATH, MACHINE_TYPE_RECORD_PATH};

const RECORD: &str = "tests/fixtures/identity/machine-type-record-v2.json";
const BINDING: &str = "tests/fixtures/identity/instance-binding-v1.json";
const L4_HOST: &str = "../test/platform/host_identity/ubuntu2404-x86-l4-g2s8.json";
/// The reserved instance id every published fixture uses.
const SYNTHETIC_INSTANCE_ID: &str = "1234567890123456789";
const ANOTHER_INSTANCE_ID: &str = "1234567890123456790";
const BOOT: &str = "00000000-0000-4000-8000-000000000001";
const ANOTHER_BOOT: &str = "00000000-0000-4000-8000-000000000002";

fn read(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The committed L4 host on the fixture boot, as a Compute Engine instance
/// with neither metadata answer: the offline case.
fn offline_l4() -> HostSources {
    let fixture: Value = serde_json::from_str(&read(L4_HOST)).expect("host fixture parses");
    let text = |key: &str| {
        fixture["sources"]
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    HostSources {
        uname_machine: text("uname_machine"),
        os_release: text("os_release"),
        cpuinfo: text("cpuinfo"),
        proc_meminfo: text("proc_meminfo"),
        pci_devices: text("pci_devices"),
        dmi_product_name: Some("Google Compute Engine\n".to_string()),
        boot_id: Some(format!("{BOOT}\n")),
        machine_type_record: Some(read(RECORD)),
        ..HostSources::default()
    }
}

/// The same host with both metadata answers: the online case.
fn online_l4(instance_id: &str) -> HostSources {
    let fixture: Value = serde_json::from_str(&read(L4_HOST)).expect("host fixture parses");
    HostSources {
        gce_machine_type: fixture["sources"]["gce_machine_type"]
            .as_str()
            .map(str::to_string),
        gce_instance_id: Some(format!("{instance_id}\n")),
        machine_type_record: None,
        ..offline_l4()
    }
}

/// The binding fixture with one field replaced.
fn binding_with(field: &str, value: Value) -> String {
    let mut binding: Value = serde_json::from_str(&read(BINDING)).expect("binding parses");
    binding[field] = value;
    serde_json::to_string_pretty(&binding).expect("serialize")
}

fn detected(sources: &HostSources) -> Result<(String, MachineTypeSource), PlatformProbeError> {
    identify(sources).map(|report| {
        (
            report.identity.machine_type.expect("a machine type"),
            report.exact.machine_type_source.expect("a source"),
        )
    })
}

#[test]
fn the_schema_2_record_is_read_and_written_byte_for_byte() {
    let body = read(RECORD);
    let record = MachineTypeRecord::parse(&body).expect("the predecessor's schema parses");
    assert_eq!(record.schema_version, 2);
    assert_eq!(record.to_json().expect("serializes"), body);
    let offline = offline_l4();
    let rewritten = MachineTypeRecord::for_live_sources(&online_l4(SYNTHETIC_INSTANCE_ID))
        .expect("facts are readable")
        .expect("a live answer records");
    assert_eq!(
        rewritten.to_json().expect("serializes"),
        body,
        "a start in the same boot rewrites the same bytes"
    );
    assert_eq!(
        detected(&offline).expect("the record alone establishes the machine type"),
        (
            "g2-standard-8".to_string(),
            MachineTypeSource::RecordedFromMetadata
        ),
        "without a binding the record decides, as it did in 0.2.1"
    );
}

#[test]
fn the_binding_fixture_is_bound_to_the_record_fixture() {
    let body = read(BINDING);
    let binding = InstanceBinding::parse(&body).expect("the binding parses");
    assert_eq!(
        binding.instance_id, SYNTHETIC_INSTANCE_ID,
        "published fixtures carry the reserved synthetic instance id"
    );
    let record_body = read(RECORD);
    let record = MachineTypeRecord::parse(&record_body).expect("record");
    assert_eq!(
        binding.machine_type_record_sha256,
        sha256_hex(record_body.as_bytes())
    );
    assert_eq!(
        InstanceBinding::new(SYNTHETIC_INSTANCE_ID.to_string(), &record, &record_body)
            .to_json()
            .expect("serializes"),
        body,
        "the writer produces the fixture"
    );
}

#[test]
fn malformed_bindings_are_refused() {
    let mut extra: Value = serde_json::from_str(&read(BINDING)).expect("binding parses");
    extra["instance_name"] = json!("host-1");
    let mut missing: Value = serde_json::from_str(&read(BINDING)).expect("binding parses");
    missing.as_object_mut().expect("object").remove("boot_id");
    let cases = [
        ("not JSON", "{".to_string()),
        ("an unknown field", extra.to_string()),
        ("a missing field", missing.to_string()),
        (
            "another schema version",
            binding_with("schema_version", json!(2)),
        ),
        (
            "a padded instance id",
            binding_with("instance_id", json!("01234")),
        ),
        (
            "a signed instance id",
            binding_with("instance_id", json!("+1234")),
        ),
        (
            "an empty instance id",
            binding_with("instance_id", json!("")),
        ),
        (
            "an instance id past u64",
            binding_with("instance_id", json!("18446744073709551616")),
        ),
        (
            "an instance id with whitespace",
            binding_with("instance_id", json!("1234 ")),
        ),
        (
            "a non-canonical machine type",
            binding_with("machine_type", json!("G2-standard-8")),
        ),
        (
            "a boot id that is not a UUID",
            binding_with("boot_id", json!("boot")),
        ),
        (
            "an uppercase digest",
            binding_with(
                "machine_type_record_sha256",
                json!(sha256_hex(b"x").to_uppercase()),
            ),
        ),
        (
            "a short digest",
            binding_with("machine_type_record_sha256", json!("abc")),
        ),
    ];
    for (label, body) in cases {
        let reason = InstanceBinding::parse(&body).expect_err(label);
        assert!(
            !reason.contains(SYNTHETIC_INSTANCE_ID),
            "{label}: the reason never quotes the file: {reason}"
        );
    }
}

/// `detail` ends with the reprovisioning steps, naming both identity files:
/// the journal line that reports a refusal is all an operator may read.
fn assert_reprovisions(label: &str, detail: &str) {
    for step in [
        "stop tensorplate-agent",
        INSTANCE_BINDING_PATH,
        MACHINE_TYPE_RECORD_PATH,
        "start it while the metadata service is reachable",
    ] {
        assert!(detail.contains(step), "{label}: says `{step}`: {detail}");
    }
}

/// A binding naming this instance on `machine_type`, from `boot`.
fn binding_on(machine_type: &str, boot: &str) -> String {
    binding_with("boot_id", json!(boot)).replace("g2-standard-8", machine_type)
}

/// `result` is the machine-type refusal, naming both machine types and no
/// instance id.
fn assert_machine_type_changed(
    label: &str,
    result: Result<(String, MachineTypeSource), PlatformProbeError>,
) {
    let Err(err) = result else {
        panic!("{label}: expected MachineTypeChanged, got {result:?}")
    };
    let said = err.to_string();
    let PlatformProbeError::MachineTypeChanged {
        source_name,
        detail,
    } = err
    else {
        panic!("{label}: expected MachineTypeChanged, got {err:?}")
    };
    assert_eq!(source_name, INSTANCE_BINDING_PATH, "{label}");
    assert!(
        detail.contains("`g2-standard-8`") && detail.contains("`g2-standard-4`"),
        "{label}: names both machine types: {detail}"
    );
    assert_reprovisions(label, &detail);
    assert!(
        said.contains("was recorded on another machine type"),
        "{label}: {said}"
    );
    assert!(
        !said.contains(SYNTHETIC_INSTANCE_ID) && !said.contains(ANOTHER_INSTANCE_ID),
        "{label}: no instance id is echoed: {said}"
    );
}

#[test]
fn without_the_service_a_binding_from_this_boot_must_agree_with_the_record() {
    let accepted = [
        ("the binding written with the record", read(BINDING)),
        (
            "a binding from an earlier boot naming the record's machine type",
            binding_on("g2-standard-8", ANOTHER_BOOT),
        ),
    ];
    for (label, binding) in accepted {
        let sources = HostSources {
            instance_binding: Some(binding),
            ..offline_l4()
        };
        assert_eq!(
            detected(&sources).unwrap_or_else(|e| panic!("{label}: {e}")),
            (
                "g2-standard-8".to_string(),
                MachineTypeSource::RecordedFromMetadata
            ),
            "{label}"
        );
    }

    let refused = [
        (
            "another machine type in this boot",
            binding_with("machine_type", json!("g2-standard-4")),
            "names machine type `g2-standard-4`",
        ),
        (
            "another record in this boot",
            binding_with(
                "machine_type_record_sha256",
                json!(sha256_hex(b"another record")),
            ),
            "is not the one the instance binding",
        ),
        (
            "an unusable binding",
            "{}".to_string(),
            "the instance binding is unusable",
        ),
    ];
    for (label, binding, reason) in refused {
        let sources = HostSources {
            instance_binding: Some(binding),
            ..offline_l4()
        };
        match detected(&sources) {
            Err(PlatformProbeError::IdentityUnestablished {
                source_name,
                detail,
            }) => {
                assert_eq!(source_name, INSTANCE_BINDING_PATH, "{label}");
                assert!(detail.contains(reason), "{label}: {detail}");
                assert!(!detail.contains(SYNTHETIC_INSTANCE_ID), "{label}: {detail}");
            }
            other => panic!("{label}: expected IdentityUnestablished, got {other:?}"),
        }
    }

    // A binding from an earlier boot on another machine type: only a record
    // the 0.2.1 agent wrote after a rollback and a resize gets here, and the
    // online start would refuse the same way.
    let resized = HostSources {
        instance_binding: Some(binding_on("g2-standard-4", ANOTHER_BOOT)),
        ..offline_l4()
    };
    let result = detected(&resized);
    if let Err(PlatformProbeError::MachineTypeChanged { detail, .. }) = &result {
        assert!(
            detail.contains("or the disk was moved"),
            "offline, a resize cannot be told from a moved disk: {detail}"
        );
    }
    assert_machine_type_changed("an earlier boot on another machine type", result);
}

#[test]
fn with_the_service_answering_another_instance_or_machine_type_is_refused() {
    let live = ("g2-standard-8".to_string(), MachineTypeSource::GceMetadata);
    let accepted = [
        ("no binding yet", None),
        ("the binding for this instance", Some(read(BINDING))),
        (
            "a binding from an earlier boot of this instance",
            Some(binding_with("boot_id", json!(ANOTHER_BOOT))),
        ),
        (
            "an unusable binding, which this start replaces",
            Some("{}".to_string()),
        ),
    ];
    for (label, binding) in accepted {
        let sources = HostSources {
            instance_binding: binding,
            ..online_l4(SYNTHETIC_INSTANCE_ID)
        };
        assert_eq!(
            detected(&sources).unwrap_or_else(|e| panic!("{label}: {e}")),
            live,
            "{label}"
        );
    }

    let moved = HostSources {
        instance_binding: Some(read(BINDING)),
        ..online_l4(ANOTHER_INSTANCE_ID)
    };
    match detected(&moved) {
        Err(err @ PlatformProbeError::InstanceChanged { .. }) => {
            let said = err.to_string();
            assert_reprovisions("another instance", &said);
            assert!(said.contains(INSTANCE_BINDING_PATH), "{said}");
            assert!(
                !said.contains(SYNTHETIC_INSTANCE_ID) && !said.contains(ANOTHER_INSTANCE_ID),
                "no instance id is echoed: {said}"
            );
        }
        other => panic!("expected InstanceChanged, got {other:?}"),
    }
    // The binding from an earlier boot names another instance: still refused.
    let moved_across_a_boot = HostSources {
        instance_binding: Some(binding_with("boot_id", json!(ANOTHER_BOOT))),
        ..online_l4(ANOTHER_INSTANCE_ID)
    };
    assert!(matches!(
        detected(&moved_across_a_boot),
        Err(PlatformProbeError::InstanceChanged { .. })
    ));
    // Another instance and another machine type: the disk moved, which is
    // what the operator has to know first.
    let moved_and_resized = HostSources {
        instance_binding: Some(binding_on("g2-standard-4", ANOTHER_BOOT)),
        ..online_l4(ANOTHER_INSTANCE_ID)
    };
    assert!(matches!(
        detected(&moved_and_resized),
        Err(PlatformProbeError::InstanceChanged { .. })
    ));

    // This instance on another machine type: refused before anything is
    // written, from this boot and from an earlier one. A resize takes a stop
    // and a start, so the earlier boot is the case that happens.
    for (label, binding) in [
        (
            "this boot on another machine type",
            binding_on("g2-standard-4", BOOT),
        ),
        (
            "an earlier boot on another machine type, after a resize",
            binding_on("g2-standard-4", ANOTHER_BOOT),
        ),
    ] {
        let sources = HostSources {
            instance_binding: Some(binding),
            ..online_l4(SYNTHETIC_INSTANCE_ID)
        };
        assert_machine_type_changed(label, detected(&sources));
    }

    for answer in ["", "0x1f", "01234", "-1", "instance-1"] {
        let sources = HostSources {
            instance_binding: Some(read(BINDING)),
            ..online_l4(answer)
        };
        assert!(
            matches!(
                detected(&sources),
                Err(PlatformProbeError::Unrecognized { .. })
            ),
            "`{answer}` is not an instance id"
        );
    }
}
