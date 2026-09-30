// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use tensorplate_platform::memory_sampler::sampling::{
    run_samples, MemoryCollector, MemoryIo, SampleClock, SamplePlan,
};
use tensorplate_platform::memory_sampler::ProcessIdentity;
use tensorplate_protocol::{BudgetDomainName, MemoryObservation, MemorySource, ProcessRole};

const XML: &str = include_str!("../../test/platform/memory_observation/nvidia-smi-q-x.xml");
const GPU: &str = include_str!("../../test/platform/memory_observation/nvidia-smi-query-gpu.csv");
const APPS: &str =
    include_str!("../../test/platform/memory_observation/nvidia-smi-query-compute-apps.csv");
const MEMINFO: &str = include_str!("../../test/platform/memory_observation/proc-meminfo.txt");
const STATUS: &str =
    include_str!("../../test/platform/memory_observation/proc-status-python-sidecar.txt");
const PROCESS: ProcessIdentity = ProcessIdentity {
    pid: 5819,
    role: ProcessRole::PythonSidecar,
};

struct Clock(Rc<Cell<Duration>>);
impl SampleClock for Clock {
    fn elapsed(&self) -> Duration {
        self.0.get()
    }
    fn sleep_until(&mut self, at: Duration) {
        self.0.set(self.0.get().max(at));
    }
}

struct Recordings {
    xml: Option<String>,
    calls: Vec<Vec<String>>,
}
impl MemoryIo for Recordings {
    fn nvidia_smi(&mut self, args: &[&str]) -> Option<String> {
        self.calls.push(args.iter().map(|s| (*s).into()).collect());
        match args[0] {
            "-q" => self.xml.clone(),
            "--query-gpu=uuid,memory.total,memory.used,memory.free" => Some(GPU.into()),
            "--query-compute-apps=gpu_uuid,pid,used_memory" => Some(APPS.into()),
            _ => panic!("unexpected command"),
        }
    }
    fn proc_file(&mut self, path: &str) -> Option<String> {
        match path {
            "/proc/meminfo" => Some(MEMINFO.into()),
            "/proc/5819/status" => Some(STATUS.into()),
            _ => panic!("unexpected proc source"),
        }
    }
}

#[test]
fn recorded_inputs_flow_through_each_tick_and_one_shared_window_reducer() {
    // These are replays of model-free idle captures, not measured load peaks.
    for (domain, xml, source, consumed, process_bytes) in [
        (
            BudgetDomainName::GuestRam,
            Some(XML.to_owned()),
            MemorySource::Proc,
            1_940_568 * 1024,
            734_756 * 1024,
        ),
        (
            BudgetDomainName::DeviceVram,
            Some(XML.to_owned()),
            MemorySource::NvidiaSmiXml,
            726 * 1024 * 1024,
            248 * 1024 * 1024,
        ),
        (
            BudgetDomainName::DeviceVram,
            Some(XML.replace("<free>22308 MiB</free>", "")),
            MemorySource::NvidiaSmiCsv,
            726 * 1024 * 1024,
            248 * 1024 * 1024,
        ),
    ] {
        let plan = SamplePlan::new(
            Duration::from_secs(1),
            Duration::from_millis(3500),
            vec![domain],
            vec![PROCESS],
        )
        .unwrap();
        let mut clock = Clock(Rc::new(Cell::new(Duration::ZERO)));
        let mut collector = MemoryCollector::new(Recordings {
            xml,
            calls: Vec::new(),
        });
        let mut output = Vec::new();
        let report = run_samples(&plan, &mut clock, &mut collector, &mut output).unwrap();
        let observations: Vec<MemoryObservation> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(observations.len(), 4);
        assert_eq!(
            observations
                .iter()
                .map(|s| s.sampled_monotonic_ns)
                .collect::<Vec<_>>(),
            vec![0, 1_000_000_000, 2_000_000_000, 3_000_000_000]
        );
        assert!(observations
            .iter()
            .all(|s| s.source == source && s.domain == domain));
        assert_eq!(report.expected_ticks, 4);
        assert_eq!(report.completed_ticks, 4);
        assert!(report.complete);
        assert_eq!(report.domains[0].consumed.max_bytes, Some(consumed));
        assert_eq!(report.domains[0].consumed.available_samples, 4);
        assert_eq!(
            report.domains[0].processes[0].peak.max_bytes,
            Some(process_bytes)
        );
        assert_eq!(report.domains[0].processes[0].peak.available_samples, 4);
        // Both warm-idle and load windows call this reducer; the phase is supplied
        // by the operator, never inferred from a fixture or the smallest sample.
    }
}

struct Sequence {
    xml: std::collections::VecDeque<Option<String>>,
    clock: Rc<Cell<Duration>>,
    delay: Duration,
}
impl MemoryIo for Sequence {
    fn nvidia_smi(&mut self, args: &[&str]) -> Option<String> {
        if args == ["-q", "-x"] {
            self.clock.set(self.clock.get() + self.delay);
            self.xml.pop_front().expect("unexpected extra sample")
        } else {
            None
        }
    }
    fn proc_file(&mut self, _: &str) -> Option<String> {
        None
    }
}

#[test]
fn missing_samples_and_unlisted_pids_never_become_zero_or_complete_peaks() {
    for captures in [
        vec![
            Some(XML.into()),
            None,
            Some(XML.replace("22308 MiB", "22300 MiB")),
        ],
        vec![None, None, None],
    ] {
        let time = Rc::new(Cell::new(Duration::ZERO));
        let mut collector = MemoryCollector::new(Sequence {
            xml: captures.clone().into(),
            clock: time.clone(),
            delay: Duration::ZERO,
        });
        let plan = SamplePlan::new(
            Duration::from_secs(1),
            Duration::from_secs(3),
            vec![BudgetDomainName::DeviceVram],
            vec![
                PROCESS,
                ProcessIdentity {
                    pid: 42,
                    role: ProcessRole::ServingWorker,
                },
            ],
        )
        .unwrap();
        let mut output = Vec::new();
        let report = run_samples(&plan, &mut Clock(time), &mut collector, &mut output).unwrap();
        assert!(!report.complete);
        assert_eq!(report.completed_ticks, 3);
        let available = if captures[0].is_some() { 2 } else { 0 };
        assert_eq!(report.domains[0].consumed.available_samples, available);
        assert_eq!(
            report.domains[0].consumed.max_bytes,
            (available > 0).then_some(734 * 1024 * 1024)
        );
        assert_eq!(
            report.domains[0].processes[0].peak.available_samples,
            available
        );
        assert_eq!(report.domains[0].processes[1].peak.max_bytes, None);
        assert_eq!(report.domains[0].processes[1].peak.available_samples, 0);
        let samples: Vec<MemoryObservation> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(samples[1].device_aggregate.consumed_bytes(), None);
    }
}

#[test]
fn shared_reducer_keeps_the_maximum_without_adding_rss_or_driver_reservations() {
    use tensorplate_platform::memory_sampler::{parse_nvidia_smi_xml, sampling::WindowReducer};
    let plan = SamplePlan::new(
        Duration::from_secs(1),
        Duration::from_secs(3),
        vec![BudgetDomainName::DeviceVram],
        vec![PROCESS],
    )
    .unwrap();
    let mut reducer = WindowReducer::new(&plan);
    for free in [22300, 22200, 22308] {
        let sample = parse_nvidia_smi_xml(
            &XML.replace("22308 MiB", &format!("{free} MiB")),
            &[PROCESS],
            0,
        )
        .unwrap();
        reducer.observe(&sample).unwrap();
    }
    let report = reducer.finish(3, 3, 0);
    assert!(report.complete);
    assert_eq!(
        report.domains[0].consumed.max_bytes,
        Some(834 * 1024 * 1024)
    );
    assert_eq!(
        report.domains[0].processes[0].peak.max_bytes,
        Some(248 * 1024 * 1024)
    );
    let mut foreign_role = parse_nvidia_smi_xml(XML, &[], 0).unwrap();
    let mut role_reducer = WindowReducer::new(&plan);
    role_reducer.observe(&foreign_role).unwrap();
    let role_report = role_reducer.finish(1, 1, 0);
    assert!(!role_report.complete);
    assert_eq!(role_report.domains[0].processes[0].peak.max_bytes, None);
    foreign_role.sampled_monotonic_ns = u64::MAX;
    assert!(WindowReducer::new(&plan).observe(&foreign_role).is_err());
    let mut invalid =
        tensorplate_platform::memory_sampler::parse_proc_memory(None, &[], 0).unwrap();
    let mut reducer = WindowReducer::new(&plan);
    assert!(reducer.observe(&invalid).is_err());
    invalid.availability = tensorplate_protocol::ObservationAvailability::Available;
    assert!(reducer.observe(&invalid).is_err());
}

#[test]
fn slow_collection_skips_overdue_ticks_and_marks_coverage_incomplete() {
    let time = Rc::new(Cell::new(Duration::ZERO));
    let mut collector = MemoryCollector::new(Sequence {
        xml: vec![Some(XML.into()), Some(XML.into())].into(),
        clock: time.clone(),
        delay: Duration::from_millis(1500),
    });
    let plan = SamplePlan::new(
        Duration::from_secs(1),
        Duration::from_secs(4),
        vec![BudgetDomainName::DeviceVram],
        vec![PROCESS],
    )
    .unwrap();
    let mut output = Vec::new();
    let report = run_samples(&plan, &mut Clock(time.clone()), &mut collector, &mut output).unwrap();
    assert_eq!(report.expected_ticks, 4);
    assert_eq!(report.completed_ticks, 2);
    assert_eq!(report.late_ticks, 2);
    assert!(!report.complete);
    assert_eq!(time.get(), Duration::from_secs(4));
    let samples: Vec<MemoryObservation> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(
        samples
            .iter()
            .map(|s| s.sampled_monotonic_ns)
            .collect::<Vec<_>>(),
        vec![0, 2_000_000_000]
    );
}

#[test]
fn domain_set_emits_each_domain_once_and_refuses_bad_plans() {
    let domains = vec![BudgetDomainName::GuestRam, BudgetDomainName::DeviceVram];
    let plan = SamplePlan::new(
        Duration::from_secs(2),
        Duration::from_secs(1),
        domains.clone(),
        vec![PROCESS],
    )
    .unwrap();
    let mut collector = MemoryCollector::new(Recordings {
        xml: Some(XML.into()),
        calls: Vec::new(),
    });
    let mut output = Vec::new();
    let time = Rc::new(Cell::new(Duration::ZERO));
    let report = run_samples(&plan, &mut Clock(time.clone()), &mut collector, &mut output).unwrap();
    assert!(report.complete);
    assert_eq!(report.completed_ticks, 1);
    assert_eq!(time.get(), Duration::from_secs(1));
    let samples: Vec<MemoryObservation> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(
        samples.iter().map(|s| s.domain).collect::<Vec<_>>(),
        domains
    );
    assert_eq!(collector.0.calls, vec![vec!["-q", "-x"]]);
    for (interval, duration) in [
        (0, 1),
        (1, 0),
        (1, 100_001),
        (1000, 86_400_001),
        (86_400_001, 1000),
    ] {
        assert!(SamplePlan::new(
            Duration::from_millis(interval),
            Duration::from_millis(duration),
            domains.clone(),
            vec![]
        )
        .is_err());
    }
    for invalid in [
        vec![],
        vec![BudgetDomainName::SharedPool],
        vec![BudgetDomainName::GuestRam; 2],
        vec![BudgetDomainName::DeviceVram; 3],
    ] {
        assert!(SamplePlan::new(
            Duration::from_secs(1),
            Duration::from_secs(1),
            invalid,
            vec![]
        )
        .is_err());
    }
    for invalid in [
        vec![ProcessIdentity { pid: 0, ..PROCESS }],
        vec![PROCESS; 2],
        (1..=65)
            .map(|pid| ProcessIdentity { pid, ..PROCESS })
            .collect(),
    ] {
        assert!(SamplePlan::new(
            Duration::from_secs(1),
            Duration::from_secs(1),
            domains.clone(),
            invalid
        )
        .is_err());
    }
}

#[test]
fn output_failure_stops_collection_instead_of_reporting_complete_evidence() {
    struct Broken {
        flush_only: bool,
    }
    impl std::io::Write for Broken {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.flush_only {
                Ok(bytes.len())
            } else {
                Err(std::io::ErrorKind::WriteZero.into())
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::ErrorKind::WriteZero.into())
        }
    }
    let mut collector = MemoryCollector::new(Recordings {
        xml: Some(XML.into()),
        calls: Vec::new(),
    });
    let plan = SamplePlan::new(
        Duration::from_secs(1),
        Duration::from_secs(2),
        vec![BudgetDomainName::DeviceVram],
        vec![],
    )
    .unwrap();
    assert!(run_samples(
        &plan,
        &mut Clock(Rc::new(Cell::new(Duration::ZERO))),
        &mut collector,
        &mut Broken { flush_only: false }
    )
    .is_err());
    assert_eq!(collector.0.calls.len(), 1);
    assert!(run_samples(
        &plan,
        &mut Clock(Rc::new(Cell::new(Duration::ZERO))),
        &mut collector,
        &mut Broken { flush_only: true }
    )
    .is_err());
    assert_eq!(collector.0.calls.len(), 2);
}

#[test]
fn a_full_count_of_late_samples_is_still_incomplete() {
    let time = Rc::new(Cell::new(Duration::ZERO));
    let mut collector = MemoryCollector::new(Sequence {
        xml: vec![Some(XML.into()); 3].into(),
        clock: time.clone(),
        delay: Duration::from_secs(1),
    });
    let plan = SamplePlan::new(
        Duration::from_secs(1),
        Duration::from_secs(3),
        vec![BudgetDomainName::DeviceVram],
        vec![],
    )
    .unwrap();
    let report = run_samples(&plan, &mut Clock(time), &mut collector, &mut Vec::new()).unwrap();
    assert_eq!(report.completed_ticks, 3);
    assert_eq!(report.late_ticks, 3);
    assert!(!report.complete);
}
