// SPDX-License-Identifier: Apache-2.0

//! Operator sampling; blocking source calls have no hard execution deadline.

use std::io::Write;
use std::time::{Duration, Instant};

use serde::Serialize;
use tensorplate_protocol::{BudgetDomainName, ObservationAvailability, ProcessRole};

use super::{
    parse_nvidia_smi_csv, parse_nvidia_smi_xml, parse_proc_memory, ByteObservation, DomainMemory,
    MemoryObservation, MemorySampleError, MemorySource, ProcStatus, ProcessAggregate,
    ProcessIdentity, UnavailableReason,
};

#[derive(Debug, thiserror::Error)]
pub enum SamplingError {
    #[error("invalid sampling input: {0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Sample(#[from] MemorySampleError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Bounds operator output and reducer state, independently of admission policy.
#[derive(Clone, Debug)]
pub struct SamplePlan {
    interval: Duration,
    duration: Duration,
    domains: Vec<BudgetDomainName>,
    processes: Vec<ProcessIdentity>,
}
impl SamplePlan {
    pub fn new(
        interval: Duration,
        duration: Duration,
        domains: Vec<BudgetDomainName>,
        processes: Vec<ProcessIdentity>,
    ) -> Result<Self, SamplingError> {
        if interval.is_zero()
            || duration.is_zero()
            || duration > Duration::from_secs(86_400)
            || interval > Duration::from_secs(86_400)
            || duration.as_nanos().div_ceil(interval.as_nanos()) > 100_000
        {
            return Err(SamplingError::Invalid(
                "positive interval/duration, at most 24 hours and 100000 ticks required",
            ));
        }
        if domains.is_empty()
            || domains.len() > 2
            || domains.contains(&BudgetDomainName::SharedPool)
            || (domains.len() == 2 && domains[0] == domains[1])
        {
            return Err(SamplingError::Invalid(
                "select guest_ram and/or device_vram exactly once",
            ));
        }
        if processes.len() > 64 || super::roles(&processes).is_err() {
            return Err(SamplingError::Invalid(
                "at most 64 unique positive attribution PIDs required",
            ));
        }
        Ok(Self {
            interval,
            duration,
            domains,
            processes,
        })
    }
}

/// Monotonic session clock. `sleep_until` must not return before its deadline.
pub trait SampleClock {
    fn elapsed(&self) -> Duration;
    fn sleep_until(&mut self, at: Duration);
}
pub struct MonotonicClock(Instant);
impl Default for MonotonicClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl SampleClock for MonotonicClock {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep_until(&mut self, at: Duration) {
        if let Some(wait) = at.checked_sub(self.elapsed()) {
            std::thread::sleep(wait);
        }
    }
}

/// Injectable command/proc boundary; errors and invalid UTF-8 are unavailable.
pub trait MemoryIo {
    fn nvidia_smi(&mut self, args: &[&str]) -> Option<String>;
    fn proc_file(&mut self, path: &str) -> Option<String>;
}
pub struct SystemMemoryIo;
impl MemoryIo for SystemMemoryIo {
    fn nvidia_smi(&mut self, args: &[&str]) -> Option<String> {
        let output = std::process::Command::new("nvidia-smi")
            .args(args)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8(output.stdout).ok()
    }
    fn proc_file(&mut self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
}

pub struct MemoryCollector<P>(pub P);
impl<P: MemoryIo> MemoryCollector<P> {
    pub fn new(probe: P) -> Self {
        Self(probe)
    }
    pub fn collect(
        &mut self,
        domain: BudgetDomainName,
        processes: &[ProcessIdentity],
        at: u64,
    ) -> Result<MemoryObservation, MemorySampleError> {
        let mut source = MemorySource::Proc;
        let result = match domain {
            BudgetDomainName::GuestRam => {
                let meminfo = self.0.proc_file("/proc/meminfo");
                let statuses: Vec<_> = processes
                    .iter()
                    .map(|p| self.0.proc_file(&format!("/proc/{}/status", p.pid)))
                    .collect();
                let inputs: Vec<_> = processes
                    .iter()
                    .zip(&statuses)
                    .map(|(process, status)| ProcStatus {
                        process: *process,
                        status: status.as_deref(),
                    })
                    .collect();
                parse_proc_memory(meminfo.as_deref(), &inputs, at)
            }
            BudgetDomainName::DeviceVram => {
                if let Some(xml) = self.0.nvidia_smi(&["-q", "-x"]) {
                    if let Ok(sample) = parse_nvidia_smi_xml(&xml, processes, at) {
                        if sample.availability == ObservationAvailability::Available {
                            return Ok(sample);
                        }
                    }
                }
                source = MemorySource::NvidiaSmiCsv;
                let gpu = self.0.nvidia_smi(&[
                    "--query-gpu=uuid,memory.total,memory.used,memory.free",
                    "--format=csv",
                ]);
                let apps = self.0.nvidia_smi(&[
                    "--query-compute-apps=gpu_uuid,pid,used_memory",
                    "--format=csv",
                ]);
                parse_nvidia_smi_csv(
                    gpu.as_deref().unwrap_or_default(),
                    apps.as_deref(),
                    processes,
                    at,
                )
            }
            BudgetDomainName::SharedPool => {
                return Err(MemorySampleError::Inconsistent(
                    "unsupported sampling domain",
                ))
            }
        };
        // Never turn a failed read/parse into a zero or a nominal capacity.
        result.or_else(|_| {
            let missing = ByteObservation::unavailable(UnavailableReason::SourceUnavailable);
            MemoryObservation::new(
                domain,
                source,
                DomainMemory {
                    capacity: missing,
                    available: missing,
                    used: missing,
                    driver_reserved: missing,
                },
                ProcessAggregate::Unavailable {
                    reason: UnavailableReason::SourceUnavailable,
                },
                at,
            )
            .map_err(MemorySampleError::Inconsistent)
        })
    }
}

/// Maximum among observed samples, not a continuous-time or allocator peak.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SamplePeak {
    pub max_bytes: Option<u64>,
    pub available_samples: u64,
}
impl SamplePeak {
    fn observe(&mut self, value: Option<u64>) {
        if let Some(bytes) = value {
            self.max_bytes = Some(self.max_bytes.map_or(bytes, |old| old.max(bytes)));
            self.available_samples += 1;
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct ProcessPeak {
    pub pid: u32,
    pub role: ProcessRole,
    pub peak: SamplePeak,
}
#[derive(Clone, Debug, Serialize)]
pub struct DomainPeak {
    pub domain: BudgetDomainName,
    pub consumed: SamplePeak,
    pub processes: Vec<ProcessPeak>,
}
#[derive(Clone, Debug, Serialize)]
pub struct SampleReport {
    pub expected_ticks: u64,
    pub completed_ticks: u64,
    pub late_ticks: u64,
    pub complete: bool,
    pub domains: Vec<DomainPeak>,
}

/// Shared window reduction for qualification and future agent observations.
/// Only explicitly attributed PIDs are retained; external PID churn cannot grow it.
pub struct WindowReducer(Vec<DomainPeak>);
impl WindowReducer {
    pub fn new(plan: &SamplePlan) -> Self {
        Self(
            plan.domains
                .iter()
                .map(|domain| DomainPeak {
                    domain: *domain,
                    consumed: SamplePeak::default(),
                    processes: plan
                        .processes
                        .iter()
                        .map(|p| ProcessPeak {
                            pid: p.pid,
                            role: p.role,
                            peak: SamplePeak::default(),
                        })
                        .collect(),
                })
                .collect(),
        )
    }
    pub fn observe(&mut self, sample: &MemoryObservation) -> Result<(), SamplingError> {
        sample.validate().map_err(SamplingError::Invalid)?;
        let domain = self
            .0
            .iter_mut()
            .find(|d| d.domain == sample.domain)
            .ok_or(SamplingError::Invalid("unexpected sample domain"))?;
        domain
            .consumed
            .observe(sample.device_aggregate.consumed_bytes());
        for process in &mut domain.processes {
            let value = match &sample.process_aggregate {
                ProcessAggregate::Available { processes } => processes
                    .iter()
                    .find(|p| p.pid == process.pid && p.role == process.role)
                    .and_then(|p| p.bytes.bytes()),
                ProcessAggregate::Unavailable { .. } => None,
            };
            process.peak.observe(value);
        }
        Ok(())
    }
    pub fn finish(
        self,
        expected_ticks: u64,
        completed_ticks: u64,
        late_ticks: u64,
    ) -> SampleReport {
        let complete = expected_ticks > 0
            && expected_ticks == completed_ticks
            && late_ticks == 0
            && self.0.iter().all(|d| {
                d.consumed.available_samples == expected_ticks
                    && d.processes
                        .iter()
                        .all(|p| p.peak.available_samples == expected_ticks)
            });
        SampleReport {
            expected_ticks,
            completed_ticks,
            late_ticks,
            complete,
            domains: self.0,
        }
    }
}

/// Start at tick zero, stop starting batches at duration, and skip overdue ticks.
/// A batch emits one record per domain with its start timestamp; blocking I/O
/// may overrun the duration. Late or skipped batches cannot yield complete coverage.
pub fn run_samples<P: MemoryIo, C: SampleClock, W: Write>(
    plan: &SamplePlan,
    clock: &mut C,
    collector: &mut MemoryCollector<P>,
    output: &mut W,
) -> Result<SampleReport, SamplingError> {
    let start = clock.elapsed();
    let mut next = Duration::ZERO;
    let mut completed = 0;
    let mut late = 0;
    let mut reducer = WindowReducer::new(plan);
    loop {
        let wake = start
            .checked_add(next.min(plan.duration))
            .ok_or(SamplingError::Invalid("clock overflow"))?;
        clock.sleep_until(wake);
        let elapsed = clock
            .elapsed()
            .checked_sub(start)
            .ok_or(SamplingError::Invalid("clock moved backwards"))?;
        if elapsed >= plan.duration {
            break;
        }
        let timestamp = u64::try_from(elapsed.as_nanos())
            .map_err(|_| SamplingError::Invalid("timestamp overflow"))?;
        for domain in &plan.domains {
            let sample = collector.collect(*domain, &plan.processes, timestamp)?;
            reducer.observe(&sample)?;
            serde_json::to_writer(&mut *output, &sample)?;
            output.write_all(b"\n")?;
        }
        output.flush()?;
        completed += 1;
        let after = clock
            .elapsed()
            .checked_sub(start)
            .ok_or(SamplingError::Invalid("clock moved backwards"))?;
        if after >= (next + plan.interval).min(plan.duration) {
            late += 1;
        }
        next += plan.interval;
        if after > next {
            let ticks = after.as_nanos().div_ceil(plan.interval.as_nanos());
            next = Duration::from_nanos(
                u64::try_from(ticks * plan.interval.as_nanos())
                    .map_err(|_| SamplingError::Invalid("clock overflow"))?,
            );
        }
    }
    let expected = u64::try_from(plan.duration.as_nanos().div_ceil(plan.interval.as_nanos()))
        .map_err(|_| SamplingError::Invalid("tick overflow"))?;
    Ok(reducer.finish(expected, completed, late))
}
