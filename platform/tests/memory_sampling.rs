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
