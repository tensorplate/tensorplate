// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use tensorplate_platform::memory_sampler::{
    parse_nvidia_smi_csv, parse_nvidia_smi_xml, parse_proc_memory, MemorySampleError, ProcStatus,
    ProcessIdentity,
};
use tensorplate_protocol::{
    ByteObservation, MemoryObservation, MemorySource, ObservationAvailability, ProcessAggregate,
    ProcessRole, UnavailableReason,
};

const XML: &str = include_str!("../../test/platform/memory_observation/nvidia-smi-q-x.xml");
const GPU: &str = include_str!("../../test/platform/memory_observation/nvidia-smi-query-gpu.csv");
const APPS: &str =
    include_str!("../../test/platform/memory_observation/nvidia-smi-query-compute-apps.csv");
const MEMINFO: &str = include_str!("../../test/platform/memory_observation/proc-meminfo.txt");
const AGENT: &str = include_str!("../../test/platform/memory_observation/proc-status-agent.txt");
const WORKER: &str =
    include_str!("../../test/platform/memory_observation/proc-status-serving-worker.txt");
const SIDECAR: &str =
    include_str!("../../test/platform/memory_observation/proc-status-python-sidecar.txt");
const SIDECAR_ID: ProcessIdentity = ProcessIdentity {
    pid: 5819,
    role: ProcessRole::PythonSidecar,
};
const MIB: u64 = 1024 * 1024;

fn processes(observation: &MemoryObservation) -> &[tensorplate_protocol::ProcessMemory] {
    let ProcessAggregate::Available { processes } = &observation.process_aggregate else {
        panic!("processes unavailable");
    };
    processes
}

#[test]
fn recorded_xml_replays_the_idle_observation_fixture() {
    let sample = parse_nvidia_smi_xml(XML, &[SIDECAR_ID], 1_000_000_000).unwrap();
    let fixture: MemoryObservation = serde_json::from_str(include_str!(
        "../../protocol/rust/tests/fixtures/memory_observation_l4_idle.json"
    ))
    .unwrap();
    assert_eq!(sample, fixture);
    assert_eq!(sample.device_aggregate.consumed_bytes(), Some(726 * MIB));
    assert_eq!(sample.device_aggregate.used.bytes(), Some(256 * MIB));
    assert_eq!(
        sample.device_aggregate.driver_reserved.bytes(),
        Some(471 * MIB)
    );
    assert_eq!(processes(&sample)[0].bytes.bytes(), Some(248 * MIB));
    assert_eq!(sample.allocated.bytes(), None);
    assert_eq!(sample.reserved.bytes(), None);
}

#[test]
fn recorded_csv_is_an_independent_observation_without_a_reserved_reading() {
    let sample = parse_nvidia_smi_csv(GPU, Some(APPS), &[SIDECAR_ID], 123).unwrap();
    assert_eq!(sample.source, MemorySource::NvidiaSmiCsv);
    assert_eq!(sample.sampled_monotonic_ns, 123);
    assert_eq!(sample.availability, ObservationAvailability::Available);
    assert_eq!(sample.device_aggregate.capacity.bytes(), Some(23034 * MIB));
    assert_eq!(sample.device_aggregate.available.bytes(), Some(22308 * MIB));
    assert_eq!(
        sample.device_aggregate.driver_reserved,
        ByteObservation::unavailable(UnavailableReason::MissingField)
    );
    assert_eq!(processes(&sample)[0].role, ProcessRole::PythonSidecar);
    assert_eq!(processes(&sample)[0].bytes.bytes(), Some(248 * MIB));
    let changed =
        parse_nvidia_smi_csv(&GPU.replace("256 MiB", "257 MiB"), Some(APPS), &[], 123).unwrap();
    assert_eq!(changed.device_aggregate.used.bytes(), Some(257 * MIB));
    assert_eq!(processes(&changed)[0].role, ProcessRole::External);
}

#[test]
fn proc_recordings_use_available_and_rss_with_explicit_pid_attribution() {
    let sample = parse_proc_memory(
        Some(MEMINFO),
        &[
            ProcStatus {
                process: ProcessIdentity {
                    pid: 5795,
                    role: ProcessRole::Agent,
                },
                status: Some(AGENT),
            },
            ProcStatus {
                process: ProcessIdentity {
                    pid: 5818,
                    role: ProcessRole::ServingWorker,
                },
                status: Some(WORKER),
            },
            ProcStatus {
                process: SIDECAR_ID,
                status: Some(SIDECAR),
            },
        ],
        0,
    )
    .unwrap();
    assert_eq!(sample.source, MemorySource::Proc);
    assert_eq!(sample.availability, ObservationAvailability::Available);
    assert_eq!(
        sample.device_aggregate.capacity.bytes(),
        Some(32_858_380 * 1024)
    );
    assert_eq!(
        sample.device_aggregate.available.bytes(),
        Some(30_917_812 * 1024)
    );
    assert_eq!(
        sample.device_aggregate.consumed_bytes(),
        Some(1_940_568 * 1024)
    );
    let values: Vec<_> = processes(&sample)
        .iter()
        .map(|p| (p.pid, p.role, p.bytes.bytes()))
        .collect();
    assert_eq!(
        values,
        vec![
            (5795, ProcessRole::Agent, Some(4460 * 1024)),
            (5818, ProcessRole::ServingWorker, Some(5116 * 1024)),
            (5819, ProcessRole::PythonSidecar, Some(734_756 * 1024))
        ]
    );
    let changed_hwm = SIDECAR.replace("VmHWM:\t  734756 kB", "VmHWM:\t  999999 kB");
    assert_ne!(changed_hwm, SIDECAR);
    let unchanged_rss = parse_proc_memory(
        Some(MEMINFO),
        &[ProcStatus {
            process: SIDECAR_ID,
            status: Some(&changed_hwm),
        }],
        0,
    )
    .unwrap();
    assert_eq!(
        processes(&unchanged_rss)[0].bytes.bytes(),
        Some(734_756 * 1024)
    );
}

#[test]
fn missing_fields_and_sources_stay_unavailable_never_zero() {
    let xml = XML
        .replace("<free>22308 MiB</free>", "")
        .replace("<used_memory>248 MiB</used_memory>", "");
    let sample = parse_nvidia_smi_xml(&xml, &[], 0).unwrap();
    assert_eq!(sample.availability, ObservationAvailability::Partial);
    assert_eq!(sample.device_aggregate.available.bytes(), None);
    assert_eq!(processes(&sample)[0].bytes.bytes(), None);
    assert_eq!(sample.device_aggregate.consumed_bytes(), None);
    let csv = parse_nvidia_smi_csv(GPU, None, &[], 0).unwrap();
    assert_eq!(csv.availability, ObservationAvailability::Partial);
    assert_eq!(
        csv.process_aggregate,
        ProcessAggregate::Unavailable {
            reason: UnavailableReason::SourceUnavailable
        }
    );
    let meminfo = MEMINFO
        .lines()
        .filter(|l| !l.starts_with("MemAvailable:"))
        .collect::<Vec<_>>()
        .join("\n");
    let status = SIDECAR
        .lines()
        .filter(|l| !l.starts_with("VmRSS:"))
        .collect::<Vec<_>>()
        .join("\n");
    let proc = parse_proc_memory(
        Some(&meminfo),
        &[ProcStatus {
            process: SIDECAR_ID,
            status: Some(&status),
        }],
        0,
    )
    .unwrap();
    assert_eq!(proc.device_aggregate.available.bytes(), None);
    assert_eq!(processes(&proc)[0].bytes.bytes(), None);
    let unavailable = parse_proc_memory(
        None,
        &[ProcStatus {
            process: SIDECAR_ID,
            status: None,
        }],
        0,
    )
    .unwrap();
    assert_eq!(unavailable.device_aggregate.capacity.bytes(), None);
    assert_eq!(
        processes(&unavailable)[0].bytes,
        ByteObservation::unavailable(UnavailableReason::SourceUnavailable)
    );
}

#[test]
fn unavailable_driver_values_preserve_the_other_readings() {
    for token in ["N/A", "[N/A]", "Not Supported", "[Not Supported]"] {
        let sample = parse_nvidia_smi_xml(&XML.replace("248 MiB", token), &[], 0).unwrap();
        assert_eq!(sample.availability, ObservationAvailability::Partial);
        assert_eq!(
            processes(&sample)[0].bytes,
            ByteObservation::unavailable(UnavailableReason::Unsupported)
        );
        assert_eq!(sample.device_aggregate.capacity.bytes(), Some(23034 * MIB));
    }
}

#[test]
fn malformed_numeric_fields_fail_for_xml_csv_and_proc() {
    for bad in [
        "-1 MiB",
        "1.5 MiB",
        "1e2 MiB",
        "1 MB",
        "1 MiB tail",
        "18446744073709551615 MiB",
        "8589934592 MiB",
        "",
        "garbage",
    ] {
        assert!(
            parse_nvidia_smi_xml(&XML.replace("248 MiB", bad), &[], 0).is_err(),
            "XML {bad}"
        );
        assert!(
            parse_nvidia_smi_csv(GPU, Some(&APPS.replace("248 MiB", bad)), &[], 0).is_err(),
            "CSV {bad}"
        );
        let value = bad.replace("MiB", "kB");
        // Values overflowing MiB can still fit in KiB; test the source's own bound.
        if bad != "8589934592 MiB" {
            assert!(
                parse_proc_memory(Some(&MEMINFO.replace("30917812 kB", &value)), &[], 0).is_err(),
                "proc {value}"
            );
        }
    }
}

#[test]
fn malformed_structure_ambiguity_and_cross_capture_identity_are_rejected() {
    for text in [
        XML.replace("</gpu>", ""),
        XML.replace("</nvidia_smi_log>", "<gpu/></nvidia_smi_log>"),
        XML.replace("<attached_gpus>1", "<attached_gpus>2"),
        XML.replace("<pid>5819</pid>", "<pid>5819</pid><pid>5820</pid>"),
        XML.replace(
            "<used_memory>248 MiB</used_memory>",
            "<used_memory><value>248 MiB</value></used_memory>",
        ),
        XML.replace("<pid>5819</pid>", "<pid>0</pid>"),
        XML.replace("23034 MiB", "1 MiB"),
    ] {
        assert!(parse_nvidia_smi_xml(&text, &[], 0).is_err());
    }
    let second_gpu = format!("{GPU}{}\n", GPU.lines().nth(1).unwrap());
    assert_eq!(
        parse_nvidia_smi_csv(&second_gpu, Some(APPS), &[], 0),
        Err(MemorySampleError::DeviceCount)
    );
    assert!(parse_nvidia_smi_csv(
        GPU,
        Some(&APPS.replace("000000000005", "000000000006")),
        &[],
        0
    )
    .is_err());
    assert!(parse_nvidia_smi_csv(
        GPU,
        Some(&format!("{APPS}{}\n", APPS.lines().nth(1).unwrap())),
        &[],
        0
    )
    .is_err());
    assert!(parse_nvidia_smi_csv(
        &GPU.replace("index, name", "name, name"),
        Some(APPS),
        &[],
        0
    )
    .is_err());
    assert!(parse_nvidia_smi_csv(&GPU.replace("0, NVIDIA", "NVIDIA"), Some(APPS), &[], 0).is_err());
    assert!(parse_nvidia_smi_xml(XML, &[SIDECAR_ID, SIDECAR_ID], 0).is_err());
    assert!(parse_proc_memory(
        Some(MEMINFO),
        &[ProcStatus {
            process: SIDECAR_ID,
            status: Some(AGENT)
        }],
        0
    )
    .is_err());
    assert!(parse_proc_memory(Some(&format!("{MEMINFO}MemAvailable: 1 kB\n")), &[], 0).is_err());
    assert_eq!(
        parse_nvidia_smi_xml(&" ".repeat(4 * 1024 * 1024 + 1), &[], 0),
        Err(MemorySampleError::Oversized)
    );
}

#[test]
fn empty_process_table_is_not_a_failed_query() {
    let apps = format!("{}\n", APPS.lines().next().unwrap());
    let empty = parse_nvidia_smi_csv(GPU, Some(&apps), &[], 0).unwrap();
    assert_eq!(empty.availability, ObservationAvailability::Available);
    assert!(processes(&empty).is_empty());
    let start = XML.find("<process_info>").unwrap();
    let end = XML.find("</process_info>").unwrap() + "</process_info>".len();
    let mut xml = XML.to_owned();
    xml.replace_range(start..end, "");
    assert!(processes(&parse_nvidia_smi_xml(&xml, &[], 0).unwrap()).is_empty());
    xml = xml
        .replace("<processes>", "<unavailable_processes>")
        .replace("</processes>", "</unavailable_processes>");
    assert_eq!(
        parse_nvidia_smi_xml(&xml, &[], 0)
            .unwrap()
            .process_aggregate,
        ProcessAggregate::Unavailable {
            reason: UnavailableReason::MissingField
        }
    );
}

#[test]
fn xml_node_count_is_bounded_independently_of_capture_bytes() {
    let accepted = XML.replace(
        "</nvidia_smi_log>",
        &format!("{}</nvidia_smi_log>", "<unused/>".repeat(1000)),
    );
    assert_eq!(
        parse_nvidia_smi_xml(&accepted, &[], 0).unwrap(),
        parse_nvidia_smi_xml(XML, &[], 0).unwrap()
    );
    let expanded = XML.replace(
        "</nvidia_smi_log>",
        &format!("{}</nvidia_smi_log>", "<unused/>".repeat(100_001)),
    );
    assert!(expanded.len() < 4 * 1024 * 1024);
    assert_eq!(
        parse_nvidia_smi_xml(&expanded, &[], 0),
        Err(MemorySampleError::Malformed("invalid XML"))
    );
}
